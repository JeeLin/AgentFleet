<script setup lang="ts">
/* ===========================================================================
   SettingsPage —— 设置（规范 §5.3：M2 范围，只定结构）
   ---------------------------------------------------------------------------
   §5.3 把这一页标成 M2、「只定结构，控件全禁用」。所以这里**没有一项能改**：
   每个控件都禁用，旁边写明它 M2 会做什么。

   ── 为什么不放几行「看起来能用」的输入框 ─────────────────────────
   一个能打字、按保存却没反应的设置页，比一张全是禁用控件的表更糟：
   用户会以为保存失败，然后反复试。禁用 + 一句「M2」是唯一诚实的形态。

   ── 这一页不显示任何 secret ──────────────────────────────────────
   约束 13：secret 值永不回显。设备密钥指纹在设备页逐台显示，
   这里连一行假的 `sk-…` 都不放——那会让人以为某处真存了一个 key。
   =========================================================================== */

import { navigate } from '@/router'

interface Row {
  title: string
  hint: string
  /** 控件的占位文字。**不是**真实值。 */
  placeholder: string
  /** M2 时这一项会做什么。 */
  m2: string
  type?: 'text' | 'password'
}

const ROWS: readonly Row[] = [
  {
    title: 'Hub 连接',
    hint: '绑定地址、TLS、心跳间隔。',
    placeholder: 'M2 起可改',
    m2: '改绑定地址与 TLS 需要重启 Hub；保存前会先说明影响，不静默重启。',
  },
  {
    title: '改密码',
    hint: '单用户唯一凭据。',
    placeholder: 'M2 起可改',
    m2: '改完其它已登录会话不失效——会话绑定凭据版本，改密只递增版本。',
    type: 'password',
  },
  {
    title: '模型 API key',
    hint: 'agent 运行时使用的密钥。',
    placeholder: 'M2 起可填',
    m2: '只写不读：保存后只显示前 3 位与末 3 位；换 key 不重启已有会话。',
    type: 'password',
  },
  {
    title: '节点凭据',
    hint: '每台设备自己的身份密钥。',
    placeholder: '按设备显示指纹',
    m2: '这里是展示窗口，不是保险箱：只列指纹，私钥永不上传（约束 11）。',
  },
]
</script>

<template>
  <div class="af-page af-settings">
    <div class="af-page__head">
      <div>
        <h1 class="af-page__title">设置</h1>
        <p class="af-page__sub">
          M2 范围。这一页先定结构：<b>没有任何一项现在能改</b>，每个控件都是禁用的，
          旁边写明 M2 时会做什么。列在这里是因为导航需要知道它存在，不是为了先画一个假表单。
        </p>
      </div>
      <span class="af-spacer"></span>
      <span class="af-badge af-badge--off">M2 起可改</span>
    </div>

    <div class="af-settings__rows">
      <section v-for="row in ROWS" :key="row.title" class="af-settings__row">
        <div class="af-settings__head">
          <h2 class="af-settings__title">{{ row.title }}</h2>
          <p class="af-field__hint">{{ row.hint }}</p>
        </div>
        <div class="af-settings__body">
          <input
            class="af-input"
            :type="row.type ?? 'text'"
            :placeholder="row.placeholder"
            disabled
            :aria-label="`${row.title}（M2 起可改）`"
          />
          <p class="af-field__hint">{{ row.m2 }}</p>
        </div>
      </section>
    </div>

    <div class="af-settings__foot">
      <p class="af-field__hint">
        M1 的凭据只有一处：设备侧的密钥对。它在设备上生成、私钥永不上传，
        Hub 只存指纹——在
        <button class="af-link" type="button" @click="navigate('/devices')">设备页</button>
        逐台可见。
      </p>
      <p class="af-field__hint">
        这里不列「任务分发」「推理网关」这类 M5+ 的开关：留一个灰掉的导航项等于在导航里
        承诺一件本阶段做不到的事。
      </p>
    </div>
  </div>
</template>

<style scoped>
.af-settings {
  max-width: 900px;
}
.af-settings__rows {
  display: flex;
  flex-direction: column;
}
.af-settings__row {
  display: flex;
  align-items: flex-start;
  gap: var(--af-space-6);
  padding: var(--af-space-5) 0;
  border-bottom: 1px solid var(--af-border-subtle);
}
.af-settings__row:last-child {
  border-bottom: 0;
}
.af-settings__head {
  flex: 0 0 220px;
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
}
.af-settings__title {
  margin: 0;
  font-size: var(--af-text-sm);
  font-weight: var(--af-weight-medium);
  color: var(--af-fg-primary);
}
.af-settings__body {
  flex: 1 1 auto;
  max-width: 560px;
  display: flex;
  flex-direction: column;
  gap: var(--af-space-2);
}
.af-settings__body .af-input {
  width: 100%;
}
.af-settings__foot {
  margin-top: var(--af-space-8);
  display: flex;
  flex-direction: column;
  gap: var(--af-space-3);
  max-width: 70ch;
}
.af-settings__foot p {
  margin: 0;
}
@media (max-width: 900px) {
  .af-settings__row {
    flex-direction: column;
    gap: var(--af-space-3);
  }
  .af-settings__head {
    flex: 0 0 auto;
  }
}
</style>
