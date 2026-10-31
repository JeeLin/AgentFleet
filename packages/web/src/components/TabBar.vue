<script setup lang="ts">
/* ===========================================================================
   TabBar —— 多会话 tab 条（规范 §5.5）
   ---------------------------------------------------------------------------
   整条是**一个 tab stop**（roving tabindex）：只有当前 tab 的 label 是
   `tabindex="0"`，其余 `-1`。所以 Tab 进来只落在「当前会话」上，
   再按 Tab 走到它的关闭按钮，不会先穿过五个 tab。

   | `←` `→` | 移动**并激活**（像浏览器 tab 条） |
   | `Home` `End` | 两端 |
   | `Delete` | 关当前 tab |
   | `⇧Tab` | 从工作台正文回到这里（正文在 DOM 里紧跟其后，浏览器自带） |

   ── 为什么移动即激活 ─────────────────────────────────────────────
   切一个 tab 便宜且可逆（不丢任何状态），没有理由让用户多按一次；
   真正不可逆的动作（关 tab）才需要停顿。

   ── 关闭按钮必须是真 <button> ────────────────────────────────────
   它**不在** label 按钮里面：嵌在 button 里的 span/button 会被父按钮从
   Tab 序列里拿掉，于是「键盘用户能关掉一个会话」在结构上做不出来。
   关闭会释放通道、丢未保存改动，不许是鼠标专属。

   ── 这里没有 ⌃W ──────────────────────────────────────────────────
   那是浏览器的快捷键，拦不住。把一个冲突的键绑到不可逆动作上比不给键更危险。
   =========================================================================== */

import { nextTick, ref } from 'vue'

import { neighborId, type TabMeta, type TabsState } from '@/lib/tabs'
import { PANEL_ICON, PANEL_LABEL, type PanelName } from '@/lib/panel'

/** 一台 tab 的设备连接态。未选中且断开时 tab 左侧有一道 1px 色标（§5.5.7）。 */
export type TabConn = 'online' | 'reconnecting' | 'offline'

const props = withDefaults(
  defineProps<{
    tabs: TabMeta[]
    activeId: string | null
    /**
     * 每个 tab 当前开着的工具（`null` = 坞收着）。
     *
     * v0.4.0 这里传的是「面板数」——它的用途是让「N 个 tab = N × 面板数条
     * 活跃通道」这个代价可见。v0.5.0 右侧只有一格，那个数恒为 1，于是换成
     * 工具图标：「这个 tab 现在开着什么」仍然有用，而图标是最省的载体。
     */
    panelIcons: Record<string, PanelName | null>
    /** tab 所属设备的连接态。 */
    connOf: (nodeId: string | null) => TabConn
    /**
     * 哪个 tab 有未保存的改动（文件面板编辑区）。
     *
     * §5.5.2 明写「关 tab 会丢未保存改动」——这句话只有在 tab 上**看得见**
     * 时才成立，否则它只是一句写在文档里的免责声明。缺省为空：不是每个
     * 宿主都有文件编辑（列表页复用本组件时没有这个概念）。
     */
    dirty?: Record<string, boolean>
  }>(),
  { dirty: () => ({}) },
)

const emit = defineEmits<{
  (e: 'activate', id: string): void
  /** 关 tab = 生命周期边界：页面据此释放这个 tab 的全部通道。 */
  (e: 'close', id: string): void
}>()

const bar = ref<HTMLElement | null>(null)

/** 状态对象只用于 `neighborId` 的纯函数计算。 */
function state(): TabsState {
  return { tabs: props.tabs, activeId: props.activeId }
}

function labelEl(id: string): HTMLElement | null {
  if (!bar.value) return null
  return bar.value.querySelector<HTMLElement>(`[data-tab-label="${CSS.escape(id)}"]`)
}

/** 焦点落到某个 tab 的名字按钮上（关 tab 之后的落点，§5.5.5）。 */
function focusTab(id: string): void {
  void nextTick(() => labelEl(id)?.focus())
}

function activate(id: string, focus = true): void {
  emit('activate', id)
  if (focus) focusTab(id)
}

function onLabelClick(id: string, e: MouseEvent): void {
  // 中键是「在新标签页打开」——这里没有新标签页，但也不该触发别的
  if (e.button !== 0) return
  activate(id)
}

function onCloseClick(id: string): void {
  // 焦点落在哪儿由页面决定（它才知道关完之后该激活谁，§5.5.5），
  // 页面会调 `focusTab(nextId)`。
  emit('close', id)
}

function onLabelKeydown(id: string, e: KeyboardEvent): void {
  if (e.isComposing) return
  switch (e.key) {
    case 'ArrowLeft':
    case 'ArrowRight':
    case 'Home':
    case 'End': {
      const key =
        e.key === 'ArrowLeft' ? 'prev' : e.key === 'ArrowRight' ? 'next' : e.key === 'Home' ? 'first' : 'last'
      const next = neighborId(state(), id, key)
      // 到头不绕回：tab 条不是转盘。没移动就不吞键，把键还给浏览器。
      if (!next || next === id) return
      e.preventDefault()
      activate(next)
      return
    }
    case 'Delete': {
      e.preventDefault()
      emit('close', id)
      return
    }
    default:
      return
  }
}

/** 当前 tab 的 label。页面的 `⇧Tab` 回跳与「切 tab 后进正文」都用它。 */
defineExpose({ focusTab })
</script>

<template>
  <div
    ref="bar"
    class="af-tabbar"
    role="tablist"
    aria-label="打开的会话"
    aria-orientation="horizontal"
  >
    <div
      v-for="tab in tabs"
      :key="tab.id"
      class="af-tab"
      :data-sid="tab.id"
      :data-conn="connOf(tab.nodeId)"
    >
      <button
        class="af-tab__label"
        type="button"
        role="tab"
        :aria-selected="tab.id === activeId ? 'true' : 'false'"
        :tabindex="tab.id === activeId ? 0 : -1"
        :data-tab-label="tab.id"
        :title="`${tab.path}（${tab.id}）`"
        @click="onLabelClick(tab.id, $event)"
        @keydown="onLabelKeydown(tab.id, $event)"
      >
        <span class="af-tab__name">{{ tab.title }}</span>
      </button>
      <!-- 当前工具图标：切回去之前先知道会看到什么。
           v0.4.0 这里是个数字徽标（面板数）；一格之后数字恒为 1，见 props 注释。 -->
      <span
        v-if="panelIcons[tab.id]"
        class="af-tab__tool"
        data-tab-tool
        :title="`这个会话当前开着${PANEL_LABEL[panelIcons[tab.id]!]}面板`"
        aria-hidden="true"
      >{{ PANEL_ICON[panelIcons[tab.id]!] }}</span>
      <!-- 脏标记：关掉这个 tab 会丢掉未保存的改动（§5.5.2）。写在 tab 上是因为
           那份改动随 tab 一起消失，而面板在切走时已经被卸掉、说不出口。 -->
      <span
        v-if="dirty[tab.id]"
        class="af-badge af-badge--warn"
        data-tab-dirty
        title="有未保存的改动：关掉这个 tab 会丢掉它"
      >未保存</span>
      <button
        class="af-tab__x"
        type="button"
        :aria-label="`关闭会话：${tab.title}`"
        title="关闭此会话（会释放它的面板通道）"
        :data-tab-close="tab.id"
        @click.stop="onCloseClick(tab.id)"
      >
        ✕
      </button>
    </div>
  </div>
</template>
