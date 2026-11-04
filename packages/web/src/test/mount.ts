/* ===========================================================================
   组件测试公共件：一次挂载 + 清场。
   ---------------------------------------------------------------------------
   每个用例结束都把 DOM 清掉：这些组件都往 document 上挂 keydown 监听
   （会话工作台、TabBar 的键盘处理），留下一个监听器会让下个用例的按键
   被上一个已卸载的组件接走——症状是「单独跑绿、全量跑红」。
   =========================================================================== */

import { mount, type ComponentMountingOptions } from '@vue/test-utils'
import type { Component } from 'vue'
import { afterEach, vi } from 'vitest'

import type { AgentSummary } from '@/domains/agent/api'
import type { DeviceSummary } from '@/domains/device/api'
import type { SessionSummary } from '@/domains/session/api'

/**
 * 本文件挂过的组件。每个用例结束时**卸载**它们，而不是只清 DOM。
 *
 * `document.body.innerHTML = ''` 会把节点摘掉，但 Vue 的 `onBeforeUnmount`
 * 不会因此触发——而页面级组件正是在那里撤销自己注册的键位。漏掉卸载的
 * 后果是第二个用例挂载时 `keynav.register` 抛「一个动作只能有一个主人」，
 * 症状是「单独跑绿、全量跑红」。
 */
const mounted: Array<{ unmount: () => void }> = []

afterEach(() => {
  for (const w of mounted.splice(0)) w.unmount()
  document.body.innerHTML = ''
  vi.unstubAllGlobals()
})

/** 一个设备摘要，字段全给，缺省值可覆盖。 */
export function device(over: Partial<DeviceSummary> = {}): DeviceSummary {
  return {
    id: 'n_1',
    name: 'mbp.local',
    platform: 'darwin/arm64',
    node_version: '0.1.0',
    status: 'online',
    last_seen: 'unix:1700000000',
    cpu: 20,
    mem: 40,
    gpu: false,
    agents: [],
    usable_agents: [],
    derived: false,
    key_fingerprint: '9f3***21b',
    ...over,
  }
}

/**
 * 一条 agent 目录项。
 *
 * 缺省是**可用**：夹具造的是「正常情况」，而「不可用」那档要连带 `error`
 * 一起显式造出来——没有 error 的不可用在页面上不是一个合法状态。
 */
export function agent(over: Partial<AgentSummary> = {}): AgentSummary {
  return {
    name: 'opencode',
    available: true,
    error: null,
    ...over,
  }
}

/** 一个会话摘要。 */
export function session(over: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: 's_1',
    agent_id: null,
    state: 'running',
    created_at: 'unix:1700000000',
    ended_at: null,
    // 缺省给 `unknown` 而不是 `ok`：与 Hub 侧迁移默认值同一条口径——
    // 「我还不知道」不能假装成「取到了」。用例要别的态就自己 `over`。
    base_rev: null,
    base_rev_status: 'unknown',
    workspace: { node_id: 'n_1', path: '/srv/app', path_locked: true },
    ...over,
  }
}

/** 挂到一个真实的 DOM 容器上——`document.activeElement` 才成立。 */
export function mountIn<C extends Component>(
  component: C,
  options: ComponentMountingOptions<C> = {} as ComponentMountingOptions<C>,
): MountedIn<C> {
  const host = document.createElement('div')
  document.body.appendChild(host)
  const wrapper = mount(component, { ...options, attachTo: host })
  mounted.push(wrapper)
  return wrapper as MountedIn<C>
}

/**
 * `mount` 的返回类型，只把 `element` 换**紧**。
 *
 * `VueWrapper.element` 的类型是 `T['$el']`，而 Vue 的
 * `ComponentPublicInstance['$el']` 是 `any`——`VueWrapper` 的默认形参
 * （`VM = unknown`）又带不出 `$el` 的具体类型，于是 `w.element` 落到 `any`，
 * 所有 `w.element.querySelectorAll<HTMLElement>(...)` 都会被判成
 * TS2347「Untyped function calls may not accept type arguments」。
 *
 * `$el` 在运行时**总是**组件根元素，-mounted 之后必然是 `HTMLElement`
 * （单根组件的根不可能是文本节点）。所以这里断言比 `any` 更准，
 * 不是把类型放宽，是把它收紧。
 */
export type MountedIn<C extends Component> = Omit<ReturnType<typeof mount<C>>, 'element'> & {
  readonly element: HTMLElement
}
