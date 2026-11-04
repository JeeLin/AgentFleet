<script setup lang="ts">
/* ===========================================================================
   SetupPage —— 首次 setup（设计规范 §5.2 / §5.3「认证流程」）
   ---------------------------------------------------------------------------
   这个文件负责：在 Hub 还没有密码时建立唯一凭据，成功后把人送去**登录**
   （`/login`，不是 `/sessions`——`POST /setup` 明确不签发 cookie，
   见 `setup.rs:80`「不签发 cookie——由客户端接着 POST /login 换 session」）。
   与登录页一样不带外壳：登录前没有导航可言。

   ── setup token 从哪来：页面上必须写清楚，不假装能自动获取 ──────────
   token 只有两条出口（`crates/hub-server/src/lib.rs:311-318`）：
     ① Hub **启动时打到 stderr** 的那一行
        `afhub: first-run setup token (POST /setup with header x-setup-token): …`
     ② `<data>/setup-token.txt`，权限 0600（`setup.rs:294` 的
        `write_setup_token_file`）。缺省 `<data>` 是
        `$XDG_DATA_HOME/agentfleet` 或 `~/.local/share/agentfleet`，
        也可用 `AFHUB_DATA_DIR` 指定。
   **没有接口能把它取回来**，也不该有：一旦 GET 得到 token，它就从一个
   「只有本机能看到的秘密」变成了「任何能发请求的人都能要到的值」，
   而 `POST /setup` 要 token 的全部意义正是挡住「你还没打开浏览器时，
   同网段的另一台机器 curl 一下拿到整个 Hub」（`setup.rs:4-7`）。

   ── 三种失败分开呈现（§4.7 / §632 行）──────────────────────────
     setup_token_mismatch  → 重新取一次 token（旧的还在，只是不对）
     setup_token_locked    → token 已死（失败 10 次）：重启 Hub 才会重新生成
                             （启动时发现仍未初始化 → 新 token + 失败计数清零）
     already_initialized   → setup 只能做一次，直接去登录
   三句话三步路。压成一句「setup 失败」用户只能瞎试。

   ── 密码下限与强度条 ─────────────────────────────────────────────
   硬下限是 **8**（`crates/hub-core/src/auth.rs:194` 的 `password.len() < 8`），
   服务端 400 回 `password_too_short`。这里把下限显示出来、并在提交前
   自己先拦一次（省一次往返），但**强度条不是判据**：它只是提醒，
   服务端只认长度。所以它不做「必须达到 3 格才能提交」这种假规则。
   =========================================================================== */

import { computed, onMounted, ref } from 'vue'

import {
  PASSWORD_MIN_LENGTH,
  describeSetupFailure,
  fetchSetupState,
  submitSetup,
  type AuthFailure,
} from '@/lib/auth'
import { ApiError } from '@/lib/http'
import { LOGIN_PATH, navigate } from '@/router'

const token = ref('')
const password = ref('')
const submitting = ref(false)
const failure = ref<AuthFailure | null>(null)

/**
 * `GET /api/setup` 已经告诉我们「Hub 已初始化」——那就不必等用户填完再打一次。
 * 它对应 §4.7 的第三种失败（已初始化），只是提前发生。
 */
const alreadyInitialized = ref(false)
/** 连「问一句设过没有」都失败时，不装作一切正常。 */
const stateUnknown = ref(false)

onMounted(async () => {
  try {
    await fetchSetupState()
  } catch (err) {
    // 已初始化 → 409 already_initialized。这不是加载失败，是「来错页面了」。
    if (err instanceof ApiError && err.code === 'already_initialized') {
      alreadyInitialized.value = true
      return
    }
    // 其余（网络、500）不阻塞表单：用户手里的 token 可能是对的，
    // 只是这一次探路没成。写明「状态未知」，不画一个假的「一切就绪」。
    stateUnknown.value = true
  }
})

/**
 * 密码强度：**只用于提示**，不是提交判据。
 *
 * 四级阈值（8 / 12 / 14+符号 / 16+大写+数字+符号）是设计规范 §3 的口径，
 * 与后端注册时的强度校验保持一致；文案里给出「为什么该更长」而不是一个
 * 冷冰冰的分数。
 */
const strength = computed(() => {
  const v = password.value
  if (v.length >= 16 && /[A-Z]/.test(v) && /[0-9]/.test(v) && /[^a-zA-Z0-9]/.test(v)) return 4
  if (v.length >= 14 && /[^a-zA-Z0-9]/.test(v)) return 3
  if (v.length >= 12) return 2
  if (v.length >= PASSWORD_MIN_LENGTH) return 1
  return 0
})

const strengthLabel = computed(() => {
  switch (strength.value) {
    case 4:
      return '强度足够'
    case 3:
      return '够用，还可以更长'
    case 2:
      return '够用'
    case 1:
      return '达到最低长度'
    default:
      return `还差 ${Math.max(0, PASSWORD_MIN_LENGTH - password.value.length)} 位`
  }
})

const tokenReady = computed(() => token.value.trim().length > 0)
const passwordReady = computed(() => password.value.length >= PASSWORD_MIN_LENGTH)
const canSubmit = computed(() => tokenReady.value && passwordReady.value && !submitting.value)

async function submit(): Promise<void> {
  if (!canSubmit.value || submitting.value) return
  submitting.value = true
  failure.value = null
  try {
    await submitSetup(token.value.trim(), { password: password.value })
    // `setup.rs:80`：setup 不签发 cookie，session 还得靠 POST /login 换。
    // `replace`：这张表单已经用掉了（token 也已被作废），不该留在后退路径上。
    navigate(LOGIN_PATH, { replace: true })
  } catch (err) {
    failure.value = describeSetupFailure(err)
    if (!failure.value.retryable) {
      // token 被锁 / 已初始化：把 token 清掉。它已经不可能再被接受，
      // 留在框里只会让用户以为「再按一次就好了」。
      token.value = ''
      if (failure.value.goTo === LOGIN_PATH) alreadyInitialized.value = true
    }
    password.value = ''
  } finally {
    submitting.value = false
  }
}

function toLogin(): void {
  navigate(LOGIN_PATH)
}

const HINT_ID = 'setup-token-hint'
const ERR_ID = 'setup-token-error'
const PW_HINT_ID = 'setup-pw-hint'
</script>

<template>
  <main class="af-auth af-auth--setup" tabindex="-1">
    <div class="af-auth__box" style="width: min(560px, 100%)">
      <div class="af-auth__brand">
        <span class="af-brand__mark" aria-hidden="true"></span>
        <span class="af-t-label">AGENTFLEET · 首次 setup</span>
      </div>

      <div>
        <h1 class="af-auth__title">完成首次配置</h1>
        <p class="af-auth__sub" style="margin-top: var(--af-space-3)">
          Hub 现在停在<b>未初始化</b>状态：除了 setup 本身，所有接口都会回
          403 <code>hub_not_initialized</code>。设好密码之后它才进入正常模式。
        </p>
      </div>

      <!-- 已初始化：setup 只能做一次，这里不该继续填 -->
      <div v-if="alreadyInitialized" class="af-auth__note af-auth__note--info" role="alert">
        <p class="af-auth__note-title">Hub 已经初始化过了</p>
        <p>setup 只能做一次。直接去登录即可。</p>
        <button class="af-btn af-btn--primary" type="button" @click="toLogin">去登录</button>
      </div>

      <template v-else>
        <form novalidate @submit.prevent="submit">
          <div class="af-col gap-6">
            <!-- ① token 在机器上，不在浏览器里 -->
            <section class="af-field" aria-labelledby="h-token">
              <h2 id="h-token" class="af-field__label">① 在运行 Hub 的机器上取 setup token</h2>
              <p class="af-field__hint">
                token 由 Hub 启动时生成，只出现在下面两个地方——它<b>不会</b>、也<b>不能</b>从浏览器里取回：
              </p>
              <ul class="af-auth__src">
                <li>
                  Hub 启动时的 <b>stderr</b>：一行
                  <code>afhub: first-run setup token (POST /setup with header x-setup-token): …</code>
                </li>
                <li>
                  文件 <code>&lt;data&gt;/setup-token.txt</code>（权限 <code>0600</code>）。
                  <code>&lt;data&gt;</code> 缺省是 <code>~/.local/share/agentfleet</code>
                  （或 <code>$XDG_DATA_HOME/agentfleet</code>），可用
                  <code>AFHUB_DATA_DIR</code> 指定。
                </li>
              </ul>
              <p class="af-field__hint">
                它是密码，别贴进聊天工具或截图。失败 10 次即失效（防爆破）。
              </p>
            </section>

            <!-- ② 粘贴 token -->
            <div class="af-field">
              <label class="af-field__label" for="setup-token">② 粘贴 token</label>
              <input
                id="setup-token"
                v-model="token"
                class="af-input af-fp"
                type="password"
                name="setup-token"
                autocomplete="off"
                spellcheck="false"
                :disabled="submitting"
                :aria-invalid="failure && failure.code.startsWith('setup_token') ? 'true' : 'false'"
                :aria-describedby="failure ? `${HINT_ID} ${ERR_ID}` : HINT_ID"
                @input="failure = null"
              />
              <span :id="HINT_ID" class="af-field__hint">
                头名是 <code>X-Setup-Token</code>；本页会把它原样放进这一次请求里，不留存。
              </span>
              <span
                v-if="failure && failure.code.startsWith('setup_token')"
                :id="ERR_ID"
                class="af-field__error"
                role="alert"
              >
                {{ failure.next }}
                <span class="af-t-mute">（{{ failure.code }}）</span>
              </span>
            </div>

            <!-- ③ 密码 -->
            <div class="af-field">
              <label class="af-field__label" for="setup-password">③ 设置控制台密码</label>
              <input
                id="setup-password"
                v-model="password"
                class="af-input"
                type="password"
                name="new-password"
                autocomplete="new-password"
                :disabled="submitting"
                :aria-invalid="failure && failure.code === 'password_too_short' ? 'true' : 'false'"
                :aria-describedby="`${PW_HINT_ID} setup-strength`"
                @input="failure = null"
              />
              <!-- 强度条只是提示：服务端只认长度（≥ 8），别把它做成假门禁。
                   数字与文字一起给——只靠颜色/格数会把色弱用户排除在外。 -->
              <div
                id="setup-strength"
                class="pw-meter"
                :data-level="strength"
                role="meter"
                aria-valuemin="0"
                aria-valuemax="4"
                :aria-valuenow="strength"
                :aria-valuetext="strengthLabel"
              >
                <i v-for="i in 4" :key="i"></i>
              </div>
              <span :id="PW_HINT_ID" class="af-field__hint">
                {{ strengthLabel }}。至少 {{ PASSWORD_MIN_LENGTH }} 位——这是唯一凭据，没有邮箱可找回。
              </span>
              <span
                v-if="failure && failure.code === 'password_too_short'"
                class="af-field__error"
                role="alert"
              >
                {{ failure.next }}
              </span>
            </div>

            <hr class="af-hairline" />

            <button
              class="af-btn af-btn--primary af-btn--lg af-auth__submit"
              type="submit"
              :disabled="!canSubmit"
            >
              {{ submitting ? '提交中…' : '完成 setup' }}
            </button>
            <p class="af-field__hint">
              成功之后 Hub 会作废这个 token（删文件 + 清内存），并把你送去登录页——
              setup 不签发登录 cookie，session 要靠登录换。
            </p>
          </div>
        </form>

        <!-- 其余失败：token 之外的（内部错误、网络、密码太短的兜底） -->
        <div
          v-if="failure && !failure.code.startsWith('setup_token') && failure.code !== 'password_too_short'"
          class="af-auth__note"
          :class="failure.retryable ? 'af-auth__note--warn' : 'af-auth__note--danger'"
          role="alert"
        >
          <p>{{ failure.next }}</p>
          <p v-if="failure.code === 'internal_error'" class="af-t-mute">
            密码不会被写进去，Hub 仍停在初始化模式——可以重试。
          </p>
          <p v-else class="af-t-mute">（{{ failure.code }}）</p>
        </div>

        <!-- token 被锁：唯一的写法与「不匹配」分开，因为出路不同 -->
        <div v-if="failure && failure.code === 'setup_token_locked'" class="af-auth__note af-auth__note--danger" role="alert">
          <p class="af-auth__note-title">setup token 已被锁死</p>
          <p>{{ failure.next }}</p>
          <p class="af-t-mute">
            重启 Hub 后新 token 会重新出现在 stderr 与
            <code>&lt;data&gt;/setup-token.txt</code>。
          </p>
        </div>

        <hr class="af-hairline" />

        <div class="af-auth__foot">
          <p v-if="stateUnknown" class="af-auth__state" role="status">
            没能确认 Hub 的初始化状态（探路请求失败了）。表单仍可提交——
            如果 Hub 其实已经设过密码，提交会得到 <code>already_initialized</code>。
          </p>
          <p>
            已经设过密码了？
            <button class="af-link" type="button" @click="toLogin">去登录</button>
          </p>
        </div>
      </template>
    </div>
  </main>
</template>

<style scoped>
/* setup 与 login 一样是认证页，但不复用 400px 的窄列：它有一串要照抄的
   命令与路径，窄列会把它们折行——命令折行是复制的第一杀手。
   宽度在模板上按 `min(560px, 100%)` 覆盖，这里只管其余形状。 */
.af-auth__submit {
  width: 100%;
}
.af-auth__src {
  margin: 0;
  padding-left: var(--af-space-6);
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
  font-size: var(--af-text-sm);
  color: var(--af-fg-tertiary);
  line-height: var(--af-leading-base);
}
.af-auth__src code,
.af-auth__foot code,
.af-field__hint code {
  font-family: var(--af-font-mono);
  color: var(--af-fg-secondary);
  background: var(--af-surface-2);
  padding: 0 var(--af-space-1);
  border-radius: var(--af-radius-sm);
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
.af-auth__note--warn {
  border-color: var(--af-warn-line);
  border-left-color: var(--af-warn);
  background: var(--af-warn-bg);
}
.af-auth__note--danger {
  border-color: var(--af-danger-line);
  border-left-color: var(--af-danger);
  background: var(--af-danger-bg);
}
.af-auth__note p {
  margin: 0;
}
.af-auth__note-title {
  color: var(--af-fg-primary);
  font-weight: var(--af-weight-medium);
}
.af-auth__note--warn .af-auth__note-title {
  color: var(--af-warn-fg);
}
.af-auth__note--danger .af-auth__note-title {
  color: var(--af-danger-fg);
}
.af-auth__foot {
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
}
.af-auth__foot p {
  margin: 0;
}
.af-auth__state {
  color: var(--af-warn-fg);
}
</style>
