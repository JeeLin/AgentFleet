<script setup lang="ts">
/* ===========================================================================
   LoginPrompt —— session 过期时弹出的密码框（§4.7「需要别的动作」那一列）
   ---------------------------------------------------------------------------
   为什么是**弹框**而不是 `location.assign('/login')`：

   401 撞上的那一刻，用户正在某个页面做一件事（扫设备、在会话里敲终端、新建
   会话表单填了一半）。整页跳去登录页把「正在做的事」连同「我在哪个地址」
   一起丢掉，回来还得自己导航回去——而重新登录其实只需要一个密码。所以这里
   是模态：背后那页的 DOM 从头到尾没被拆掉，关掉框就是「什么都没发生过」。

   ── 为什么登录请求不走 `http.ts` ────────────────────────────────────
   `http.ts` 把**每一个 401**都算作「session 过期」——那层不知道调用方正在打
   哪个端点，也只能这么设计。而 `POST /login` 自己的 401 是
   `invalid_credentials`（**密码错**）。两者绝不能混：把「密码错」当成
   「session 过期」会变成一个死循环（弹框 → 401 → 再弹框）。

   所以登录请求走 `lib/auth.ts` 的 `postLogin`——它**直接 `fetch`**，不经过
   `http.ts`（那一层的 401 分支正是会弹框的那个分支）。登录页与这个框共用
   这一份：两份等价的 `postLogin` 分布两处时，「错误码怎么读」迟早会漂。

   ── 五种错误分开呈现 ────────────────────────────────────────────────
   文案一律来自 `lib/auth.ts` 的 `describeLoginFailure`，这里**只负责呈现**。
   两处各写一遍判断必然漂（同 `LoginPage.vue` 文件头）。

   「Hub 还没初始化」是这一框里唯一的例外：它的下一步是**去 /setup**，
   不是重试密码。这时放人去 setup——否则用户会在这儿把一个永远不会被接受的
   密码试到限流。

   ── 为什么不自动重放刚才失败的那次请求 ─────────────────────────────
   「至少恢复到弹框前的状态」靠的是**页面没被卸载**，不需要重放。而重放
   要么要求每个页面自己声明「我怎么重新加载」，要么让这一层盲目再发一次
   `POST`——后者会把一次失败的建会话 / 送字节变成可能重复的状态变更。
   两件事都不该由这个框猜，所以它只管重新登录本身。
   =========================================================================== */

import { computed, onBeforeUnmount, ref, watch } from 'vue'

import { describeLoginFailure, postLogin, type AuthFailure } from '@/lib/auth'
import { handleLayerKeydown, pop, push } from '@/lib/focus'
import { SETUP_PATH, navigate } from '@/router'

const props = defineProps<{
  /** 框开着吗。受控 prop：关它由父级（`App.vue`）复位。 */
  open: boolean
}>()

const emit = defineEmits<{
  /** 请求关闭（登录成功 / 去 setup / 取消）。父级据此复位 `open`。 */
  (e: 'close'): void
  /** 用户选择离开：**只有这一条路**会真的跳出当前页面。 */
  (e: 'go-login'): void
}>()

const password = ref('')
const submitting = ref(false)
const failure = ref<AuthFailure | null>(null)
const rootEl = ref<HTMLElement | null>(null)
/** 这一层是否已经入栈。防重复 `push`（重渲染不该再入一次栈）。 */
let pushed = false

/** 错误描述 id。`aria-describedby` 指向它，键盘与读屏都能听见。 */
const ERR_ID = 'login-prompt-error'
const HINT_ID = 'login-prompt-hint'

const canSubmit = computed(() => password.value.length > 0 && !submitting.value)

/**
 * 关框。**三条路都走这里**（登录成功 / Hub 未初始化 / 取消），
 * 所以「焦点还回去」与「出栈」只有一条实现（§4.1）。
 */
function close(): void {
  closeLayer()
  emit('close')
}

function openLayer(): void {
  const el = rootEl.value
  if (!el || pushed) return
  // 开合只走焦点栈：`push` 负责背后 inert 与首焦点，`pop` 负责还焦点。
  // 不传 opener —— 401 不是用户点某个按钮点出来的，没有「触发元素」可言，
  // 焦点栈会退到 `main[tabindex="-1"]`（那是「这里是一片内容的起点」）。
  push(el)
  pushed = true
  document.addEventListener('keydown', onKeydown)
}

function closeLayer(): void {
  const el = rootEl.value
  if (!el || !pushed) return
  pushed = false
  document.removeEventListener('keydown', onKeydown)
  pop(el)
}

/** Esc / Tab。判据都在 `handleLayerKeydown` 里（与 `TrajDrawer.vue` 共用一份）。 */
function onKeydown(e: KeyboardEvent): void {
  const root = rootEl.value
  if (!root) return
  handleLayerKeydown(e, root, close)
}

/** 点遮罩：只有点在遮罩自己上才算（点在框里不该关）。 */
function onScrim(e: MouseEvent): void {
  if (e.target === e.currentTarget) close()
}

async function submit(): Promise<void> {
  if (!canSubmit.value) return
  submitting.value = true
  failure.value = null
  try {
    await postLogin(password.value)
    // 成功：浏览器已经拿到新的 `af_session` / `af_csrf`。**不导航、不刷新**——
    // 背后那页一直活着，关掉框就够了。
    close()
  } catch (err) {
    failure.value = describeLoginFailure(err)
    // 密码清掉：错的密码留在框里既没用，又多留了一份明文在 DOM 上
    // （与 `LoginPage.vue` 同一条理由）。
    password.value = ''
  } finally {
    submitting.value = false
  }
}

/** 去 setup：Hub 还没设密码时密码试一万次也不会被接受，换一个动作。 */
function toSetup(): void {
  close()
  navigate(SETUP_PATH)
}

/** 取消：这一条**才**真的离开当前页面。 */
function cancel(): void {
  close()
  emit('go-login')
}

watch(
  () => props.open,
  (open) => {
    if (open) {
      // 框是 `v-if` 渲出来的：这一帧 `rootEl` 还是 null，所以入栈推到微任务
      // 里（与 `TrajDrawer.vue` 同一条理由）。
      void Promise.resolve().then(() => {
        if (props.open) openLayer()
      })
      return
    }
    // 父级自己关掉了这条路径也要出栈：否则焦点栈上留一个已经不存在的层，
    // 下一次 Esc 会关掉一个空层（§4.1）。
    closeLayer()
    // 顺带把表单清干净：下一次 401 弹开时该是一个空框，不是上一次的残留。
    password.value = ''
    failure.value = null
  },
  { immediate: true },
)

onBeforeUnmount(() => {
  // 开着就走一条完整的关闭路径：直接摘 DOM 会把焦点留在 body 上
  closeLayer()
})
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      ref="rootEl"
      class="af-scrim af-loginprompt"
      role="dialog"
      aria-modal="true"
      aria-label="重新登录"
      tabindex="-1"
      data-loginprompt
      @click="onScrim"
    >
      <div class="af-modal">
        <div class="af-modal__head">
          <span class="af-panel__title">登录已过期</span>
          <!-- 这句是整句中文，不是注解：它按 §2.3 的立法意图不许压到 text-2xs。
               text-2xs 的合法性只来自「不是长文本」，而这里 14 个字连成一句，
               压到 10px 就是这个产品主打的「高文字密度看不清」。删除行内样式后
               它继承 .af-modal__head 的自然字号（body 基线 13px）。 -->
          <span class="af-t-mute">
            重新登录就回到刚才那页，做过的事不会丢
          </span>
          <span class="af-spacer"></span>
          <button
            class="af-btn af-btn--ghost af-btn--sm af-btn--icon"
            type="button"
            aria-label="关闭"
            @click="close"
          >
            ✕
          </button>
        </div>

        <form class="af-modal__body af-col gap-5" novalidate @submit.prevent="submit">
          <div class="af-field">
            <label class="af-field__label" for="login-prompt-password">密码</label>
            <input
              id="login-prompt-password"
              v-model="password"
              class="af-input"
              data-autofocus
              type="password"
              name="password"
              autocomplete="current-password"
              :disabled="submitting"
              :aria-invalid="failure ? 'true' : 'false'"
              :aria-describedby="failure ? `${HINT_ID} ${ERR_ID}` : HINT_ID"
              @input="failure = null"
            />
            <span :id="HINT_ID" class="af-field__hint">
              这是唯一凭据，没有邮箱可以找回；会话过期不会改它。
            </span>
            <!-- 错误一律就地挂在输入框下：这五种里有四种的下一步是「再试一次」，
                 只有 `hub_not_initialized` 要换动作，于是给它单开一个按钮。 -->
            <span v-if="failure" :id="ERR_ID" class="af-field__error" role="alert">
              {{ failure.next }}
              <span class="af-t-mute">（{{ failure.code }}）</span>
              <button
                v-if="failure.goTo === SETUP_PATH"
                class="af-link"
                type="button"
                @click="toSetup"
              >
                去完成首次 setup
              </button>
            </span>
          </div>

          <div class="af-row gap-4">
            <button class="af-btn af-btn--primary" type="submit" :disabled="!canSubmit">
              {{ submitting ? '验证中…' : '登录' }}
            </button>
            <button class="af-btn af-btn--ghost" type="button" :disabled="submitting" @click="cancel">
              取消
            </button>
            <span class="af-spacer"></span>
            <span class="af-t-mute" style="font-size: var(--af-text-2xs)">
              <kbd>Esc</kbd> 取消
            </span>
          </div>
        </form>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
/* 布局全部复用 `components.css` §11 的 `.af-scrim` / `.af-modal` 与
   §4 的 `.af-input` / `.af-field`——这里一条新皮肤都不造，只把模态收窄一档：
   460px 而不是 560px，因为这个框里只有一个密码格，宽了只会显得空。
   层级沿用 `--af-z-modal`（60），不为了自己往上抬：键位帮助（80）压在它上面
   时 Esc 会按焦点栈把两层一起收掉，那不是死锁，是「先答完眼前这个问题」。 */
.af-loginprompt .af-modal {
  width: min(460px, 100%);
}
</style>
