<script setup lang="ts">
/* ===========================================================================
   App.vue —— 应用外壳（规范 §5.1）
   ---------------------------------------------------------------------------
   布局：图标轨 + 侧栏 + 顶栏 + 主区 + 状态条。一级导航只有 5 项，分三组。

   ── 这个文件负责的三件事 ─────────────────────────────────────────
   ① 侧栏与顶栏的 chrome；② 键位帮助面板（`?` 由 `KEYS` 渲染，不手写）；
   ③ **跳转后的焦点落点**：`main` 每次换页都拿一次焦点（§4.3）。
   页面自己不管外壳，也不重新声明 `main`。

   ④ **session 过期时的重新登录框**（`LoginPrompt`）：401 由 `lib/http.ts`
   弹框、由这里挂载与「取消时跳 /login」。这是外壳而不是页面的事——跳转是
   一个策略（见 `onLoginCancel` 的注释）。

   ── 折叠态为什么不是「藏起来」 ───────────────────────────────────
   `⌃B` 把侧栏收成 44px 图标轨，导航项仍在（`aria-label` 补偿被
   `display:none` 掉的名字）。折叠是省空间，不是删功能——所以没有
   「折叠后只剩一个汉堡菜单」这种等价路径。
   =========================================================================== */

import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'

import EmptyState from '@/components/EmptyState.vue'
import LoginPrompt from '@/components/LoginPrompt.vue'
import { pop, push } from '@/lib/focus'
import { SCOPE_LABEL, KEYS, unimplemented } from '@/lib/keynav'
import { logout } from '@/lib/api'
import { loginPromptOpen, dismissLoginPrompt } from '@/lib/login-prompt'
import { bindShell, installShellKeys } from '@/lib/shell-keys'
import { interceptLink, navigate, route, startRouter, LOGIN_PATH } from '@/router'

import AgentsPage from '@/pages/AgentsPage.vue'
import DevicesPage from '@/pages/DevicesPage.vue'
import LoginPage from '@/pages/LoginPage.vue'
import NewSessionPage from '@/pages/NewSessionPage.vue'
import SessionPage from '@/pages/SessionPage.vue'
import SessionsPage from '@/pages/SessionsPage.vue'
import SettingsPage from '@/pages/SettingsPage.vue'
import SetupPage from '@/pages/SetupPage.vue'
import { AUTH_ROUTES } from '@/router'

const PAGES = {
  sessions: SessionsPage,
  'new-session': NewSessionPage,
  session: SessionPage,
  devices: DevicesPage,
  agents: AgentsPage,
  settings: SettingsPage,
  login: LoginPage,
  setup: SetupPage,
} as const

const page = computed(() => (route.value.name === 'not-found' ? null : PAGES[route.value.name]))
const pageKey = computed(() => `${route.value.name}:${route.value.params.id ?? ''}`)

/* ---------------------------------------------------------------- 认证页 */

/**
 * 当前是不是「认证流程」页（`/login` / `/setup`）。
 *
 * 判据是 `router.ts` 的 `AUTH_ROUTES`，不是在这里写一遍
 * `name === 'login' || name === 'setup'`：那样加第三个认证页时会漏改一处，
 * 症状是「登录页里出现了侧栏导航」——而那时用户还没登录。
 *
 * 认证页**不带外壳**（§5.1 最后一条「登录前没有导航可言」）：图标轨、
 * 侧栏、顶栏、状态条全部不渲染。状态条尤其不能留——它写着「Hub 已连接」，
 * 而此刻 Hub 可能正回着 403 `hub_not_initialized`。
 */
const isAuthPage = computed(() => AUTH_ROUTES.has(route.value.name))

/* ---------------------------------------------------------------- 侧栏 */

const NAV = [
  {
    group: '会话',
    items: [
      { to: '/sessions', label: '会话', icon: '◈', hint: 'g s', match: ['sessions', 'session'] },
      { to: '/sessions/new', label: '新建会话', icon: '✚', hint: 'g n', match: ['new-session'] },
    ],
  },
  {
    group: '环境',
    items: [
      { to: '/devices', label: '设备', icon: '▣', hint: 'g d', match: ['devices'] },
      { to: '/agents', label: 'Agent 目录', icon: '☰', hint: 'g a', match: ['agents'] },
    ],
  },
  {
    group: '系统',
    items: [{ to: '/settings', label: '设置', icon: '⚙', hint: 'g ,', match: ['settings'] }],
  },
] as const

const rail = ref<'open' | 'collapsed'>('open')

function toggleRail(): void {
  rail.value = rail.value === 'open' ? 'collapsed' : 'open'
}

/** 一条导航项是否当前页。会话工作台仍高亮「会话」。 */
function isCurrent(match: readonly string[]): boolean {
  return (match as readonly string[]).includes(route.value.name)
}

function onNavClick(e: MouseEvent, anchor: HTMLAnchorElement): void {
  interceptLink(e, anchor)
}

/* ---------------------------------------------------------------- 帮助面板 */

const helpOpen = ref(false)
const helpEl = ref<HTMLElement | null>(null)
const helpColumns = computed(() => KEYS)
const missing = computed(() => unimplemented())

/* ------------------------------------------------- session 过期重新登录 */

/**
 * 用户在重新登录框里点「取消」：**这一条**才真的去 `/login`。
 *
 * 为什么是 `location.assign` 而不是 `navigate()`：`navigate` 只换 `route`
 * （`history.pushState` + 一个 ref），JS 上下文和内存里的东西全都留着。而
 * cancel 的语义正是「这一页我不要了」——session 刚过期过，SAP 里那个
 * `LoginPage` 还会接着用它自己那份过期前的状态。一次真正的换页把这些
 * 一次清干净，`/login` 从零开始读 cookie。
 *
 * 为什么跳转住在外壳，而 `http.ts` 只管弹框：整页跳转是一个**策略**，
 * 不是「401 的技术后果」——技术后果只是「接下来没有凭据可用」，那只需要
 * 一个框。把跳转放在唯一有「外壳」这个概念的地方，`lib/` 里就不会再有
 * 第二个会整页跳转的地方（要找「哪里有跳转」时只消看这里）。
 *
 * `replace` 不用：用户是**主动放弃**当前页面的，按后退该能回到它（回到一个
 * 又一次 401 的页面，比「后退键失灵」诚实）。
 */
function onLoginCancel(): void {
  location.assign(LOGIN_PATH)
}

/**
 * 主动登出：先让 Hub 清 cookie，再去 `/login`。
 *
 * **为什么必须整页跳转**：页面内存里还留着登出前拉到的数据，只换 `route`
 * 的话它们原样在那儿——`onLoginCancel` 那条注释里写过同样的理由。
 */
async function onLogout(): Promise<void> {
  try {
    await logout()
  } catch {
    // 清不掉也要走：留在页面上的人只会看到一堆 401 弹框，而 `/login`
    // 是唯一能重新拿凭据的地方。
  }
  location.assign(LOGIN_PATH)
}

function openHelp(): void {
  // 不能在这里判 `helpEl`：面板是 `v-if`，第一次调用时它还没渲染，
  // 那样写会让 `?` 与点击都变成空操作（面板永远打不开）。
  if (helpOpen.value) return
  helpOpen.value = true
  void nextTick(() => {
    if (helpEl.value) push(helpEl.value)
  })
}

function closeHelp(): boolean {
  if (!helpOpen.value) return false
  helpOpen.value = false
  // 关闭只有一条路：`pop()` 负责还焦点。这里不许自己 `el.focus()`。
  pop()
  return true
}

function onHelpBackdrop(e: MouseEvent): void {
  if (e.target === e.currentTarget) closeHelp()
}

/* ---------------------------------------------------------------- 搜索槽 */

/**
 * `/` 聚焦当前页的搜索框。页面把搜索框标成 `data-role="search"`，
 * 外壳只按这个约定找——**不引入一份「哪个页面有搜索」的表**，
 * 那张表会在下一个页面出现时立刻过期。
 */
function focusSearch(): boolean {
  const el = document.querySelector<HTMLElement>('[data-role="search"]')
  if (!el) return false
  el.focus()
  if (el instanceof HTMLInputElement) el.select()
  return true
}

/* ---------------------------------------------------------------- 生命周期 */

let stopRouter: (() => void) | null = null
let stopKeys: (() => void) | null = null

onMounted(() => {
  installShellKeys()
  stopKeys = bindShell({
    navigate: (to) => navigate(to),
    toggleRail,
    openHelp,
    closeHelp,
    focusSearch,
  })
  stopRouter = startRouter()
  // 首屏：换页的 watch 不会为初始值触发，所以这里显式落一次
  void nextTick(() => document.querySelector<HTMLElement>('main[tabindex="-1"]')?.focus())
})

onBeforeUnmount(() => {
  stopKeys?.()
  stopRouter?.()
  if (helpOpen.value) closeHelp()
})

/**
 * 换页后把焦点给 `main`（§4.3）。
 *
 * `navigate()` 里已经调用过一次 `focusMain()`，这里再跟一次是为了覆盖
 * `popstate` 与「同一路径不同参数」的情形：两种情况都不经过 `navigate` 的
 * 那次调用，而焦点必须落在新内容上。
 */
watch(
  () => route.value.path,
  () => {
    void nextTick(() => {
      const main = document.querySelector<HTMLElement>('main.af-main[tabindex="-1"]')
      if (!main) return
      const h1 = main.querySelector('h1')
      if (h1 && !h1.hasAttribute('tabindex')) h1.setAttribute('tabindex', '-1')
      main.focus()
    })
  },
)

/* ---------------------------------------------------------------- 标题 */

const docTitle = computed(() => {
  switch (route.value.name) {
    case 'sessions':
      return '会话'
    case 'new-session':
      return '新建会话'
    case 'session':
      return '会话工作台'
    case 'devices':
      return '设备'
    case 'agents':
      return 'Agent 目录'
    case 'settings':
      return '设置'
    case 'login':
      return '登录'
    case 'setup':
      return '首次 setup'
    default:
      return '找不到页面'
  }
})

watch(
  docTitle,
  (t) => {
    if (typeof document !== 'undefined') document.title = `${t} · AgentFleet`
  },
  { immediate: true },
)
</script>

<template>
  <!-- 认证流程页：只有页面本身，没有任何外壳 chrome。`main` 由页面自己给
       （`.af-auth` 就是页面根节点），所以这里不套一层 main——两层 main
       会让 `focusMain()` 的 `main[tabindex="-1"]` 选择器命中外层那个空壳。 -->
  <component :is="page" v-if="isAuthPage && page" :key="pageKey" />

  <div v-else class="af-app" :data-rail="rail">
    <div class="af-brand">
      <span class="af-brand__mark" aria-hidden="true"></span>
      <span class="af-brand__name">AGENTFLEET</span>
    </div>

    <nav class="af-sidebar" aria-label="主导航">
      <div v-for="g in NAV" :key="g.group" class="af-navgroup">
        <div class="af-navgroup__title">{{ g.group }}</div>
        <a
          v-for="item in g.items"
          :key="item.to"
          class="af-sidebar__item"
          :href="item.to"
          :aria-current="isCurrent(item.match) ? 'page' : undefined"
          :aria-label="item.label"
          :title="`${item.label}（${item.hint}）`"
          @click="onNavClick($event, $event.currentTarget as HTMLAnchorElement)"
        >
          <span class="af-sidebar__icon" aria-hidden="true">{{ item.icon }}</span>
          <span class="af-sidebar__item-text">{{ item.label }}</span>
          <span class="af-sidebar__hint" aria-hidden="true">{{ item.hint }}</span>
        </a>
      </div>
      <span class="af-spacer"></span>
      <button class="af-sidebar__close" type="button" @click="toggleRail">
        <span class="af-sidebar__icon" aria-hidden="true">⇤</span>
        <span class="af-sidebar__item-text">折叠侧栏</span>
      </button>
    </nav>

    <header class="af-topbar">
      <button
        class="af-btn af-btn--ghost af-btn--sm af-btn--icon"
        type="button"
        aria-label="折叠 / 展开侧栏"
        title="折叠侧栏（⌃B）"
        @click="toggleRail"
      >
        <span aria-hidden="true">⇥</span>
      </button>
      <h2 class="af-topbar__title">{{ docTitle }}</h2>
      <span class="af-spacer"></span>
      <button
        class="af-btn af-btn--ghost af-btn--sm af-btn--icon"
        type="button"
        aria-label="键位帮助"
        title="键位帮助（?）"
        @click="openHelp"
      >
        <span aria-hidden="true">?</span>
      </button>
      <button
        class="af-btn af-btn--ghost af-btn--sm af-btn--icon"
        type="button"
        aria-label="退出登录"
        title="退出登录"
        @click="onLogout"
      >
        <span aria-hidden="true">⏻</span>
      </button>
    </header>

    <main class="af-main" tabindex="-1">
      <component :is="page" v-if="page" :key="pageKey" />
      <EmptyState
        v-else
        glyph="◌"
        title="这个地址没有页面"
        text="检查一下链接。会话工作台是 /sessions/&lt;id&gt;，设备列表是 /devices。"
      >
        <a class="af-btn af-btn--primary" href="/sessions" @click="onNavClick($event, $event.currentTarget as HTMLAnchorElement)">
          回到会话列表
        </a>
      </EmptyState>
    </main>

    <footer class="af-statusbar">
      <span class="af-statusbar__item"><span class="af-dot" style="color: var(--af-ok)"></span>Hub 已连接</span>
      <span class="af-statusbar__item af-t-mute">单用户 · 自托管</span>
      <span class="af-spacer"></span>
      <span class="af-statusbar__item af-statusbar__key"><kbd>?</kbd> 键位</span>
      <span class="af-statusbar__item af-statusbar__key"><kbd>g</kbd> 跳转</span>
      <span class="af-statusbar__item af-statusbar__key"><kbd>⌃B</kbd> 侧栏</span>
    </footer>

    <!-- session 过期时弹出的密码框（§4.7「需要别的动作」那一列）。401 的一条
         出路是「填个密码继续」，不是「整页跳走」。它由 `lib/http.ts` 开，
         由这里挂载；取消时跳 /login 是外壳的策略（见 `onLoginCancel`）。

         它待在 `af-app` 里面而不是与它平级：外壳那个 `v-else` 的配对是 `<component
         :is="page" v-if="isAuthPage && page">`，在中间插一个 `v-if` 会抢走
         配对、整个外壳就再也不渲染了。而它自己 Teleport 到 body（焦点栈的
         「背后 inert」才成立），所以放这儿与放在外面在 DOM 上完全一样。 -->
    <LoginPrompt :open="loginPromptOpen" @close="dismissLoginPrompt" @go-login="onLoginCancel" />

    <!-- 键位帮助：最高层级，它解释其他所有。由 KEYS 渲染，不手写第二份。 -->
    <div
      v-if="helpOpen"
      ref="helpEl"
      class="af-help"
      role="dialog"
      aria-modal="true"
      aria-label="键位帮助"
      tabindex="-1"
      @click="onHelpBackdrop"
    >
      <div class="af-help__box">
        <div class="af-help__head">
          <span class="af-panel__title">键位</span>
          <span class="af-t-mute" style="font-size: var(--af-text-2xs)">
            这张表就是键位实现本身——表里有的键才有 handler
          </span>
          <span class="af-spacer"></span>
          <button class="af-btn af-btn--ghost af-btn--sm af-btn--icon" type="button" aria-label="关闭" @click="closeHelp">
            ✕
          </button>
        </div>
        <div class="af-help__body">
          <section v-for="g in helpColumns" :key="g.group">
            <div class="help__gh">
              <span>{{ g.group }}</span>
              <span class="af-t-mute">{{ SCOPE_LABEL[g.scope] }}</span>
            </div>
            <table class="help__t">
              <tbody>
                <tr v-for="k in g.keys" :key="`${k.act}:${k.keys.join('')}`" :data-unimplemented="missing.includes(k.act)">
                  <td class="kbd-cell">
                    <kbd v-for="key in k.keys" :key="key">{{ key }}</kbd>
                  </td>
                  <td>{{ k.label }}</td>
                </tr>
              </tbody>
            </table>
          </section>
        </div>
      </div>
    </div>
  </div>
</template>
