<script setup lang="ts">
/* ===========================================================================
   LoginPage —— 登录（设计规范 §5.2 / §5.3「认证流程」）
   ---------------------------------------------------------------------------
   这个文件负责：把 `POST /login` 的每一种结果翻译成**一句能照着做的话**，
   然后把人送进控制台（`INDEX_PATH`）。它不渲染外壳——登录前没有导航可言
   （§5.1 最后一条），所以 `App.vue` 对这个路由走的是无 chrome 分支。

   ── 五种情形必须分开（§4.7 定死）────────────────────────────────
   密码错 / Hub 未初始化 / 会话失效 / 429 限流 / Hub 正忙。
   它们的**下一步动作完全不同**：

     密码错        → 就地重试（错误挂在输入框上，不弹窗）
     Hub 未初始化   → 跳 setup（重试一万次也没用：那时 Hub 没有密码）
     会话失效      → 重新登录（这一次是「过期」，不是「输错」）
     429 rate_limited / login_busy → 等（说清为什么等、等的是什么）

   所以错误文案由 `lib/auth.ts` 的 `describeLoginFailure` 按 code 逐条给出，
   这里**只负责呈现**，不在这里再写一遍判断——两处判断必然漂。

   ── 为什么「未初始化」是黄色告警块而不是红色错误行 ──────────────────
   红色（§4.7 的「可重试」一侧）表示「这一格填错了」；「Hub 还没设密码」
   不是谁填错了，是**系统状态**，要换一个动作。规范给的形态是「模态解释
   面板或 sticky toast」——这一页没有模态可弹（它就是最外层），
   于是用一块带下一步链接的告警面板，并且**把提交按钮留在那儿**：
   用户可能刚在另一个标签页里做完 setup，回来直接再按一次就该成功。

   ── 不画假的加载进度，也不画假的成功态 ─────────────────────────────
   提交期间按钮文案变成「验证中…」并禁用——这就是**全部**的反馈。
   没有进度条、没有骨架、没有一个先显示出来的「已登录」再跳走：
   那些都会在 Hub 慢或失败时变成一句谎话。
   =========================================================================== */

import { computed, ref } from 'vue'

import {
  PASSWORD_MIN_LENGTH,
  describeLoginFailure,
  postLogin,
  type AuthFailure,
} from '@/lib/auth'
import { navigate, INDEX_PATH, SETUP_PATH } from '@/router'

const password = ref('')
const submitting = ref(false)
const failure = ref<AuthFailure | null>(null)

/**
 * 会话失效是**被送过来的**，不是用户按出来的。
 *
 * `http.ts` 在 401 上弹重新登录框（不是跳 `/login`），所以这一页落地时
 * 页面什么都不说，看起来像「页面自己刷新了一下」。所以这里认一下来源
 * （`?expired=1`）并显示一句说明——它属于 §4.7 的那五种之一，不该靠用户猜。
 */
const expired = computed(() => new URLSearchParams(window.location.search).get('expired') === '1')

const canSubmit = computed(() => password.value.length > 0 && !submitting.value)

async function submit(): Promise<void> {
  if (!canSubmit.value) return
  submitting.value = true
  failure.value = null
  try {
    await postLogin(password.value)
    // 成功后浏览器已经拿到 `af_session` / `af_csrf`。`replace`：登录页
    // 不该留在后退路径上——按「后退」回到一个已经登录过的表单很奇怪。
    navigate(INDEX_PATH, { replace: true })
  } catch (err) {
    failure.value = describeLoginFailure(err)
    // 清掉密码：错的密码留在框里既没用，又多留了一份明文在 DOM 上。
    password.value = ''
  } finally {
    submitting.value = false
  }
}

function toSetup(): void {
  navigate(SETUP_PATH)
}

/** 密码框的错误描述 id。`aria-describedby` 指向它，键盘与读屏都能听见。 */
const HINT_ID = 'login-hint'
const ERR_ID = 'login-error'
</script>

<template>
  <main class="af-auth" data-role="auth" tabindex="-1">
    <div class="af-auth__box">
      <div class="af-auth__brand">
        <span class="af-brand__mark" aria-hidden="true"></span>
        <span class="af-t-label">AGENTFLEET</span>
      </div>

      <div>
        <h1 class="af-auth__title">登录</h1>
        <p class="af-auth__sub" style="margin-top: var(--af-space-3)">
          单用户自托管。密码由首次 setup 设置，<b>不通过邮箱找回</b>——
          忘了就得能登上运行 Hub 的那台机器。
        </p>
      </div>

      <!-- 会话失效：说明它为什么把人送回这里（§4.7 的五种之一） -->
      <div v-if="expired && !failure" class="af-auth__note" role="status">
        <b>登录已失效。</b>cookie 过期或签名对不上了，重新登录即可。
      </div>

      <!-- 不可重试的那一类（Hub 未初始化 / 已初始化）：换动作，不是重试 -->
      <div v-if="failure && failure.goTo === SETUP_PATH" class="af-auth__note af-auth__note--warn" role="alert">
        <p class="af-auth__note-title">Hub 还没初始化</p>
        <p>{{ failure.next }}</p>
        <button class="af-btn af-btn--primary" type="button" @click="toSetup">去完成首次 setup</button>
      </div>

      <form novalidate @submit.prevent="submit">
        <div class="af-col gap-5">
          <div class="af-field">
            <label class="af-field__label" for="password">密码</label>
            <input
              id="password"
              v-model="password"
              class="af-input"
              type="password"
              name="password"
              autocomplete="current-password"
              :disabled="submitting"
              :aria-invalid="failure ? 'true' : 'false'"
              :aria-describedby="failure ? `${HINT_ID} ${ERR_ID}` : HINT_ID"
              @input="failure = null"
            />
            <span :id="HINT_ID" class="af-field__hint">
              Hub 只监听回环地址；远程访问请自己套 TLS 反代，别把明文密码发到公网上。
            </span>
            <!-- 可重试的那一类：就地呈现，不弹窗（§4.7） -->
            <span
              v-if="failure && failure.goTo !== SETUP_PATH"
              :id="ERR_ID"
              class="af-field__error"
              role="alert"
            >
              {{ failure.next }}
              <span class="af-t-mute">（{{ failure.code }}）</span>
            </span>
          </div>

          <button class="af-btn af-btn--primary af-btn--lg af-auth__submit" type="submit" :disabled="!canSubmit">
            {{ submitting ? '验证中…' : '进入控制台' }}
          </button>
        </div>
      </form>

      <hr class="af-hairline" />

      <div class="af-auth__foot">
        <p>
          第一次部署？Hub 还没设过密码时，<b>这里登录不进去</b>——
          先去
          <button class="af-link" type="button" @click="toSetup">首次 setup</button>
          建立凭据。
        </p>
        <p>
          密码至少 {{ PASSWORD_MIN_LENGTH }} 位，忘了没有邮箱可找回：
          要能登上运行 Hub 的机器才能重置（详见
          <code>docs/architecture/security-and-lifecycle.md</code>）。
        </p>
      </div>
    </div>
  </main>
</template>

<style scoped>
/* 认证页的唯一布局来自 `components.css` §18 的 `.af-auth`；这里只校正它
   没覆盖的两处：提交按钮占满一列、以及「下一步」告警块的语义色。 */
.af-auth__submit {
  width: 100%;
}
.af-auth__note {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: var(--af-space-3);
  padding: var(--af-space-4) var(--af-space-5);
  border: 1px solid var(--af-info-line);
  border-left: 2px solid var(--af-info);
  border-radius: var(--af-radius-md);
  background: var(--af-info-bg);
  color: var(--af-fg-secondary);
  font-size: var(--af-text-md);
  line-height: var(--af-leading-base);
}
/* 未初始化是「去别处」而不是「这里填错了」：warn，不是 danger（§4.7） */
.af-auth__note--warn {
  border-color: var(--af-warn-line);
  border-left-color: var(--af-warn);
  background: var(--af-warn-bg);
}
.af-auth__note p {
  margin: 0;
}
.af-auth__note-title {
  color: var(--af-warn-fg);
  font-weight: var(--af-weight-medium);
}
.af-auth__foot {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
}
.af-auth__foot p {
  margin: 0;
}
.af-auth__foot code {
  font-family: var(--af-font-mono);
  color: var(--af-fg-secondary);
}
</style>
