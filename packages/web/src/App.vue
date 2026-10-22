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

   ⑤ **整页跳转的唯一出口**（`fullPageNavigate`）：403 `hub_not_initialized`
   的跳转也住在这里（`setNotInitializedHandler`），所以 `lib/` 里没有第二个
   会整页跳转的地方。

   ── 折叠态为什么不是「藏起来」 ───────────────────────────────────
   `⌃B` 把侧栏收成 44px 图标轨，导航项仍在（`aria-label` 补偿被
   `display:none` 掉的名字）。折叠是省空间，不是删功能——所以没有
   「折叠后只剩一个汉堡菜单」这种等价路径。
   =========================================================================== */

import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'

import EmptyState from '@/components/EmptyState.vue'
import LoginPrompt from '@/components/LoginPrompt.vue'
import { pop, push, topContainer, trapTab } from '@/lib/focus'
import { SCOPE_LABEL, KEYS, hintOf, missingActs, onHandlersChanged, PREFIX_LEAD } from '@/lib/keynav'
import { logout } from '@/lib/api'
import { setNotInitializedHandler } from '@/lib/http'
import { loginPromptOpen, dismissLoginPrompt } from '@/lib/login-prompt'
import { pageTitle } from '@/lib/page-titles'
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

/**
 * 一级导航。**标签与键位提示都从 `keynav.ts` / `page-titles.ts` 读**：
 * 手抄一份的话，改了键位表而忘了改这里，侧栏就会显示一个按了没反应的键
 *（症状与「改了 `PREFIX` 而侧栏没跟着变」完全一样）。
 */
const NAV = [
  {
    group: '会话',
    items: [
      {
        to: '/sessions',
        label: pageTitle('sessions'),
        icon: '◈',
        act: 'nav:sessions',
        match: ['sessions', 'session'],
      },
      {
        to: '/sessions/new',
        label: pageTitle('new-session'),
        icon: '✚',
        act: 'nav:new-session',
        match: ['new-session'],
      },
    ],
  },
  {
    group: '环境',
    items: [
      {
        to: '/devices',
        label: pageTitle('devices'),
        icon: '▣',
        act: 'nav:devices',
        match: ['devices'],
      },
      {
        to: '/agents',
        label: pageTitle('agents'),
        icon: '☰',
        act: 'nav:agents',
        match: ['agents'],
      },
    ],
  },
  {
    group: '系统',
    items: [
      {
        to: '/settings',
        label: pageTitle('settings'),
        icon: '⚙',
        act: 'nav:settings',
        match: ['settings'],
      },
    ],
  },
]

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

/**
 * 「（未实现）」的 act 清单。
 *
 * **必须随 handler 表变化**：注册发生在组件挂载期——晚于首屏渲染。
 * 一个只读模块级 Map 的 `computed` 会在第一次求值后把结论冻住，于是
 * 后注册的 act 永远标着「（未实现）」，而用户按下去是有反应的。
 *
 * 所以这里用 `onHandlersChanged` 把「表变了」这件事接进响应式状态：
 * 表是模块级可变状态，它自己不会触发 Vue 的更新。
 */
const missing = ref<string[]>(missingActs())

/**
 * 订阅「handler 表变了」。
 *
 * **必须在这里订阅**（而不是 `onMounted`）：注册发生在子组件的挂载期，
 * 而子组件的 `onMounted` 先于父组件的 `onMounted` 跑——晚一步就漏掉了
 * 第一次注册，「（未实现）」的标记会停在首屏那一刻的结论上。
 */
const stopHandlers = onHandlersChanged(() => {
  missing.value = missingActs()
})

/**
 * Esc 与 Tab。**只在帮助是栈顶时生效**——`pop()` 按栈顶语义工作，
 * 而键盘事件是全站共用的：不判栈顶的话，帮助盖在轨迹抽屉上时一次 Esc
 * 会关掉两层。
 */
function onHelpKeydown(e: KeyboardEvent): void {
  if (e.isComposing) return
  const root = helpEl.value
  if (!root || topContainer() !== root) return
  if (e.key === 'Tab') trapTab(e, root)
}

/* ------------------------------------------------- session 过期重新登录 */

/**
 * 整页跳转的**唯一**出口。
 *
 * 整页跳转（而不是 `navigate()`）的语义是「这一页我不要了」——JS 上下文和
 * 内存里的东西全都清掉。`navigate` 只换 `route`（`history.pushState` +
 * 一个 ref），那些东西原样留在那儿。
 *
 * **为什么住在外壳**：整页跳转是一个**策略**，不是某个错误码的技术后果。
 * 401 的技术后果只是「接下来没有凭据可用」，那只需要一个框（`http.ts`
 * 弹框）；403 `hub_not_initialized` 的技术后果是「Hub 还没有用户」，那需要
 * 一整页 setup。两条路都归到同一个出口，于是「哪里会整页跳走」只有一处答案。
 */
function fullPageNavigate(path: string): void {
  location.assign(path)
}

/**
 * 403 `hub_not_initialized` 的跳转策略住在外壳。
 *
 * `http.ts` 只报事实（这是哪个错），「跳到哪」是策略——与 401 弹框同一个
 * 模式。它在 setup 体里注册（不是 `onMounted`）：首屏的请求就可能撞上
 * 403，而子组件的 `onMounted` 早于父组件的。
 */
const stopNotInitialized = setNotInitializedHandler(fullPageNavigate)

/**
 * 用户在重新登录框里点「取消」：**这一条**才真的去 `/login`。
 *
 * 取消的语义正是「这一页我不要了」——session 刚过期过，SPA 里那个
 * `LoginPage` 还会接着用它自己那份过期前的状态。一次真正的换页把这些
 * 一次清干净，`/login` 从零开始读 cookie。
 *
 * `replace` 不用：用户是**主动放弃**当前页面的，按后退该能回到它（回到一个
 * 又一次 401 的页面，比「后退键失灵」诚实）。
 */
function onLoginCancel(): void {
  fullPageNavigate(LOGIN_PATH)
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
  fullPageNavigate(LOGIN_PATH)
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
  const el = helpEl.value
  // **只在帮助是栈顶时才由它响应 Esc**：`layer:escape` 的契约是「关掉最上
  // 层，返回是否真的关了一层」，而键盘事件是全站共用的——不判栈顶的话，
  // `?` → `t` → `Esc` 会在这里关一层、又在轨迹抽屉自己的 handler 里关一层，
  // 一次按键关掉两个浮层。抽屉自己会接住属于它的那次 Esc。
  if (el && topContainer() !== el) return false
  helpOpen.value = false
  // 关闭只有一条路：`pop(el)` 负责还焦点。这里不许自己 `el.focus()`。
  //
  // **必须带上这一层**：无参 `pop()` 关的是**栈顶**，而栈顶可能不是帮助
  // （`?` → `t` → `Esc` 时栈顶是轨迹抽屉）——那会把别人的层弹出焦点栈，
  // 留下一层指向已移除元素的空壳，之后 Esc 再也关不掉任何浮层。
  if (el) pop(el)
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
  stopHandlers()
  stopNotInitialized()
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

/**
 * 浏览器标题。**页面名从 `page-titles.ts` 读**——侧栏、帮助面板、浏览器
 * 标题说的是同一批页面名，各抄一份时改一处要改三处，漏一处的症状是
 * 「侧栏叫『Agent』、浏览器标题还叫『Agent 目录』」。
 */
const docTitle = computed(() => pageTitle(route.value.name))

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
          :title="`${item.label}（${hintOf(item.act)}）`"
          @click="onNavClick($event, $event.currentTarget as HTMLAnchorElement)"
        >
          <span class="af-sidebar__icon" aria-hidden="true">{{ item.icon }}</span>
          <span class="af-sidebar__item-text">{{ item.label }}</span>
          <span class="af-sidebar__hint" aria-hidden="true">{{ hintOf(item.act) }}</span>
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
      <span class="af-statusbar__item af-statusbar__key"><kbd>{{ hintOf('help:toggle') }}</kbd> 键位</span>
      <span class="af-statusbar__item af-statusbar__key"><kbd>{{ PREFIX_LEAD }}</kbd> 跳转</span>
      <span class="af-statusbar__item af-statusbar__key"><kbd>{{ hintOf('rail:toggle') }}</kbd> 侧栏</span>
    </footer>

    <!-- session 过期时弹出的密码框（§4.7「需要别的动作」那一列）。401 的一条
         出路是「填个密码继续」，不是「整页跳走」。它由 `lib/http.ts` 开，
         由这里挂载；取消时跳 /login 是外壳的策略（见 `onLoginCancel`）。

         它待在 `af-app` 里面而不是与它平级：外壳那个 `v-else` 的配对是 `<component
         :is="page" v-if="isAuthPage && page">`，在中间插一个 `v-if` 会抢走
         配对、整个外壳就再也不渲染了。而它自己 Teleport 到 body（焦点栈的
         「背后 inert」才成立），所以放这儿与放在外面在 DOM 上完全一样。 -->
    <LoginPrompt :open="loginPromptOpen" @close="dismissLoginPrompt" @go-login="onLoginCancel" />
  </div>

  <!-- 键位帮助：最高层级，它解释其他所有。由 KEYS 渲染，不手写第二份。

       **必须 Teleport 到 body**：`focus.push` 的 `inertOthers` 只把「body 的
       直接子节点里、不含浮层容器的那几个」置 inert。帮助面板留在 `#app` 里
       时，它所在的那棵子树就是整个 `#app`——而 `#app` 是 body 唯一的子节点，
       于是「背后全部 inert」退化成「一个都没 inert」：遮罩盖住了视觉，背后的
       侧栏项仍然可 Tab、可点（`aria-modal` 与「焦点归最内层容器」都不成立）。

       它放在 `af-app` 之外，所以 `v-else` 的配对不受影响。 -->
  <Teleport to="body">
    <div
      v-if="helpOpen"
      ref="helpEl"
      class="af-help"
      role="dialog"
      aria-modal="true"
      aria-label="键位帮助"
      tabindex="-1"
      @click="onHelpBackdrop"
      @keydown="onHelpKeydown"
    >
      <div class="af-help__box">
        <div class="af-help__head">
          <span class="af-panel__title">键位</span>
          <span class="af-t-mute" style="font-size: var(--af-text-sm)">
            这张表就是键位实现本身——表里有的键才有 handler
          </span>
          <span class="af-spacer"></span>
          <button class="af-btn af-btn--ghost af-btn--sm af-btn--icon" type="button" aria-label="关闭" @click="closeHelp">
            ✕
          </button>
        </div>
        <div class="af-help__body">
          <section v-for="g in KEYS" :key="g.group">
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
  </Teleport>
</template>
