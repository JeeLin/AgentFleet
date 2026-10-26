import { describe, expect, it } from 'vitest'

import type { DeviceSummary } from './api'
import {
  deviceLabel,
  orDash,
  parseStamp,
  percent,
  relativeTime,
  sessionTitle,
  workspaceName,
} from './format'

const NOW = 1_700_000_000_000

describe('parseStamp', () => {
  it('解析 unix:<秒>', () => {
    expect(parseStamp('unix:1700000000')).toBe(1_700_000_000)
  })

  it('null / undefined / 空串都是「没有时间」', () => {
    expect(parseStamp(null)).toBeNull()
    expect(parseStamp(undefined)).toBeNull()
    expect(parseStamp('')).toBeNull()
  })

  it('ISO 时间戳不算命中——服务端给的只有一种形状', () => {
    // 这条守的是「猜错前缀会把每台设备显示成 —」：如果哪天 Hub 改了格式，
    // 页面应该显示「—」而不是把一个 ISO 串当秒数算出 1970 年。
    expect(parseStamp('2026-10-19T12:00:00Z')).toBeNull()
    expect(parseStamp('1700000000')).toBeNull()
    expect(parseStamp('unix:abc')).toBeNull()
  })
})

describe('relativeTime', () => {
  it('5 秒内是「刚刚」', () => {
    expect(relativeTime('unix:1700000000', NOW)).toBe('刚刚')
    expect(relativeTime('unix:1699999995', NOW)).toBe('刚刚')
  })

  it('按秒 / 分 / 时 / 天分段', () => {
    expect(relativeTime('unix:1699999988', NOW)).toBe('12 秒前')
    expect(relativeTime('unix:1699999700', NOW)).toBe('5 分钟前')
    expect(relativeTime('unix:1699992800', NOW)).toBe('2 小时前')
    expect(relativeTime('unix:1699913600', NOW)).toBe('1 天前')
  })

  it('未上报是「—」，不是「0 秒前」', () => {
    expect(relativeTime(null, NOW)).toBe('—')
    expect(relativeTime(undefined, NOW)).toBe('—')
    expect(relativeTime('garbage', NOW)).toBe('—')
  })

  it('服务端时钟略快时不出现负时间', () => {
    // 同一台机器上 Hub 与浏览器可能差几秒，负数会读成「-3 秒前」
    expect(relativeTime('unix:1700000060', NOW)).toBe('刚刚')
  })
})

describe('percent', () => {
  it('null 是「—」而不是 0%', () => {
    expect(percent(null)).toBe('—')
  })

  it('0 就是 0%（未上报与空转是两件事）', () => {
    expect(percent(0)).toBe('0%')
    expect(percent(37)).toBe('37%')
  })
})

describe('workspaceName / sessionTitle', () => {
  it('取路径末段', () => {
    expect(workspaceName('/srv/app')).toBe('app')
    expect(workspaceName('~/src/AgentFleet/')).toBe('AgentFleet')
  })

  it('根路径与空路径不返回空串（tab 上的名字必须能读）', () => {
    expect(workspaceName('/')).toBe('/')
    expect(workspaceName('')).toBe('（无路径）')
  })

  it('会话标题就是工作区末段——后端没有 title 字段', () => {
    const session = {
      id: 's_1',
      agent_id: null,
      state: 'running' as const,
      created_at: 'unix:1700000000',
      ended_at: null,
      workspace: { node_id: 'n_1', path: '/srv/agentfleet', path_locked: true },
    }
    expect(sessionTitle(session)).toBe('agentfleet')
  })
})

describe('deviceLabel', () => {
  // 完整的 DeviceSummary：只写 name/id 需要 `as unknown as` 强转，
  // 而强转会让「后端加了必填字段」这类改动在测试里静默通过。
  const DEVICE: DeviceSummary = {
    id: 'n_1',
    name: 'mbp.local',
    platform: 'darwin/arm64',
    node_version: '0.1.0',
    status: 'online',
    last_seen: 'unix:1700000000',
    cpu: null,
    mem: null,
    gpu: false,
    agents: [],
    usable_agents: [],
    derived: false,
    key_fingerprint: 'abc***xyz',
  }
  const DEVICES = new Map([[DEVICE.id, DEVICE]])

  it('取得到就用名字', () => {
    expect(deviceLabel(DEVICES, 'n_1')).toBe('mbp.local')
  })

  it('列表里还没有这台设备时回显 node_id（至少还能拿去搜）', () => {
    expect(deviceLabel(DEVICES, 'n_9')).toBe('n_9')
  })

  it('node_id 为 null 是「设备已移除」，不是空串、也不等于「离线」', () => {
    // null 与「列表还没回来」（上一条）必须显示成不同的话：
    // 前者设备永远不会回来了，后者等一会儿就有名字。
    expect(deviceLabel(DEVICES, null)).toBe('（设备已移除）')
    expect(deviceLabel(DEVICES, null)).not.toBe('')
  })

  it('知道原 node_id 时把它带出来（用户要能认出是哪台）', () => {
    expect(deviceLabel(DEVICES, null, 'n_old')).toBe('n_old（已移除）')
  })
})

describe('orDash', () => {
  it('取不到是「—」，不是 0（0 是一个真值，不能拿来顶「不知道」）', () => {
    expect(orDash(null)).toBe('—')
    expect(orDash(0)).toBe('0')
    expect(orDash(4821)).toBe('4821')
  })
})
