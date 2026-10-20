<script setup lang="ts">
/* ===========================================================================
   PanelDock —— 面板坞容器（规范 §5.4 / §7.1 / §7.2）
   ---------------------------------------------------------------------------
   坞里**同时**能放终端与文件，因为它们是同时看的两件事（改代码 → 跑命令 →
   看报错）。这一层只管容器：标签、状态、释放入口。面板内容由 13b / 13c
   挂进来——13a 的正文里只有一块占位，不画假的终端。

   ── 通道什么时候建：第一次交互 ────────────────────────────────────
   打开坞、切面板都**不**建通道：这时候还没有任何数据要流动，建一条空通道
   只是让设备多占一个 PTY。用户第一次点进 / 聚焦面板正文
   （`pointerdown` / `focusin` / `keydown`）时才发 `interact`，由页面去
   `open_channel`。所以这里既没有「连接」按钮，也没有自动建连。

   ── 关闭坞 = 释放它托管的面板 ─────────────────────────────────────
   关闭是一个**显式动作**，动作完成后通道就该没了（§7.2）。释放几个面板
   是页面说的，因为句柄在页面手里；这里只负责把「关闭」这个意图报上去。
   =========================================================================== */

import { computed } from 'vue'

import type { ChannelState } from '@/lib/channel'
import { CHANNEL_STATE_LABEL, channelErrorLabel } from '@/lib/channel'

export type PanelName = 'terminal' | 'files'

export interface ChannelView {
  state: ChannelState
  error: string | null
  dropped: number
}

const props = defineProps<{
  /** 当前面板。`null` 表示还没选过（坞开着但空）。 */
  panel: PanelName | null
  /** 这个 tab 已打开的面板（决定坞标签显示哪些）。 */
  mounted: PanelName[]
  /** 会话所属设备名（面板头左侧重复一次，两个终端并排时靠它分）。 */
  deviceName: string
  /** 会话工作区路径。 */
  path: string
  /** 当前面板的通道视图。 */
  channel: ChannelView
  /** 终端缓冲（纯文本；ANSI 解析归 13b）。 */
  output: string
  /** 文件面板的当前目录（相对工作区根）。 */
  cwd: readonly string[]
}>()

const emit = defineEmits<{
  (e: 'open', panel: PanelName): void
  (e: 'close'): void
  /** 第一次与面板正文交互：该建通道了。 */
  (e: 'interact', panel: PanelName): void
}>()

const PANEL_LABEL: Record<PanelName, string> = { terminal: '终端', files: '文件' }
const PANEL_ICON: Record<PanelName, string> = { terminal: '▶', files: '▤' }

/** 状态 → 徽标色。`failed` 用 danger：它是唯一需要用户处理的态。 */
const STATE_BADGE: Record<ChannelState, string> = {
  idle: 'af-badge--off',
  opening: 'af-badge--info',
  ready: 'af-badge--ok',
  failed: 'af-badge--danger',
  closed: 'af-badge--off',
}

function labelOf(name: PanelName): string {
  return PANEL_LABEL[name]
}

/**
 * 终端输出按行切开。
 *
 * 必须是 `computed` 而不是模板里调一个函数：模板里 `lines()` 会被求值两次
 * （一次 `v-for`、一次判空），每次都是一次**全量 `split`**——终端缓冲是
 * 一整段滚动记录，长输出下这两次切分随每次渲染重复。
 *
 * 空输出不画假行——占位文字会说明它为什么空。
 */
const lines = computed(() => {
  const text = props.output.replace(/\r\n/g, '\n')
  return text.length ? text.split('\n') : []
})

function onInteract(e: Event): void {
  if (!props.panel) return
  const target = e.target as HTMLElement | null
  // 点了坞标签 / 关闭按钮不是「与面板正文交互」
  if (target?.closest('.af-docktabs')) return
  emit('interact', props.panel)
}
</script>

<template>
  <div class="af-wb__dock" aria-label="面板坞">
    <div class="af-docktabs" role="tablist" aria-label="面板">
      <button
        v-for="name in mounted"
        :key="name"
        class="af-docktabs__b"
        type="button"
        role="tab"
        :aria-selected="panel === name ? 'true' : 'false'"
        :data-panel="name"
        @click="emit('open', name)"
      >
        <span aria-hidden="true">{{ PANEL_ICON[name] }}</span> {{ labelOf(name) }}
        <span v-if="panel === name && channel.state === 'ready'" class="af-badge af-badge--ok">
          已连接
        </span>
      </button>
      <span v-if="!mounted.length" class="af-docktabs__empty af-t-mute">没有打开的面板</span>
      <span class="af-docktabs__spacer"></span>
      <button
        class="af-docktabs__close"
        type="button"
        aria-label="关闭面板坞（释放面板通道）"
        @click="emit('close')"
      >
        ✕
      </button>
    </div>

    <div
      class="af-panelhost"
      tabindex="-1"
      data-panelhost
      @pointerdown="onInteract"
      @focusin="onInteract"
      @keydown="onInteract"
    >
      <div v-if="!panel" class="af-panelhost__void af-t-mute">
        从上面的标签选一个面板，或按 <kbd>o</kbd> 打开终端。
      </div>

      <template v-else>
        <div class="af-panel__head af-panelhead">
          <span class="af-panel__title">{{ labelOf(panel) }}</span>
          <span class="af-t-mute af-panelhead__where">
            <span aria-hidden="true">▣</span> {{ deviceName }}
            <span class="af-t-mute">·</span>
            <span class="af-fp">{{ path }}</span>
          </span>
          <span class="af-spacer"></span>
          <span class="af-badge" :class="STATE_BADGE[channel.state]">
            {{ CHANNEL_STATE_LABEL[channel.state] }}
          </span>
          <span
            v-if="channel.dropped > 0"
            class="af-badge af-badge--warn"
            title="断线期间丢弃的输入不排队：排队会在重连后把字符乱序灌进 PTY"
          >
            有 {{ channel.dropped }} 条已丢
          </span>
        </div>

        <!-- 建通道失败：这里给的是**为什么**与下一步，不是一个转圈 -->
        <div v-if="channel.state === 'failed'" class="af-panel__state" role="alert">
          {{ channelErrorLabel(channel.error) }}
          <span class="af-t-mute">点一下面板正文可以重试。</span>
        </div>

         <!--
           面板正文由宿主挂进来（13b / 13c）。

           这里是**具名插槽 + 13a 占位兜底**，不是直接接入组件：坞不知道
           终端与文件树的区别，把「面板里面是什么」留在这一层等于让容器
           依赖它托管的每一件东西；而留一个**没有插槽时不渲染任何东西**的
           空位也同样糟——那时「坞开着但面板没挂」看起来就是「面板坏了」。
           兜底内容说清「它还没挂进来」，与 M1 别处的空态同一口径
           （§3.12：空态回答「这里本来该有东西但没有」）。
         -->
         <slot name="terminal">
           <template v-if="panel === 'terminal'">
             <div
               class="af-term"
               tabindex="0"
               role="log"
               aria-label="终端输出（纯文本，xterm 渲染归 13b）"
               data-panel-focus
             >
               <div v-for="(line, i) in lines" :key="i" class="af-term__line">{{ line }}</div>
               <div v-if="!lines.length" class="af-term__line af-term__dim">
                 终端内容由 13b 挂入（xterm.js）。点这里会建立通道；之后设备上的输出会出现在这一块。
               </div>
             </div>
             <p class="af-panelhost__note af-field__hint">
               通道在第一次交互时建立：这个面板还没动过时是「未建立」。
               关掉面板坞才会 close_channel；切 tab 不释放。
             </p>
           </template>
         </slot>

         <slot name="files">
           <div v-if="panel === 'files'" class="af-fpane">
             <div class="af-fpane__crumbs">
               <b>{{ path }}</b>
               <template v-for="seg in cwd" :key="seg">
                 <span class="af-t-mute"> / </span><b>{{ seg }}</b>
               </template>
             </div>
             <div class="af-fpane__body af-panelhost__void af-t-mute">
               文件树与预览由 13c 挂入。工作区根就是会话的路径——文件面板没有第二个根。
             </div>
           </div>
         </slot>
      </template>
    </div>
  </div>
</template>

<style scoped>
.af-wb__dock {
  /* 宽度由页面按分栏比例给（flex-basis），这里只保证能收窄 */
  min-width: 240px;
}
.af-docktabs__empty {
  display: flex;
  align-items: center;
  padding: 0 var(--af-space-4);
  font-size: var(--af-text-xs);
}
.af-panelhead {
  border-left: 0;
  border-right: 0;
  border-top: 0;
}
.af-panelhead__where {
  display: inline-flex;
  align-items: center;
  gap: var(--af-space-2);
  min-width: 0;
  font-size: var(--af-text-xs);
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}
.af-panelhost__void {
  display: flex;
  align-items: center;
  justify-content: center;
  flex: 1 1 auto;
  padding: var(--af-space-6);
  font-size: var(--af-text-xs);
  text-align: center;
  line-height: var(--af-leading-base);
}
.af-panelhost__note {
  flex: 0 0 auto;
  margin: 0;
  padding: var(--af-space-2) var(--af-space-4);
  border-top: 1px solid var(--af-border-subtle);
  background: var(--af-surface-1);
}
.af-fpane__crumbs {
  flex: 0 0 auto;
}
.af-fpane__body {
  display: flex;
  align-items: center;
  justify-content: center;
}
</style>
