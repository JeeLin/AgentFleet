import { describe, expect, it } from 'vitest'
import type { ProxyOptions } from 'vite'

import { devProxy } from '../../dev-proxy.config'

/**
 * `vite.config.ts` 的 dev 代理是**后端契约的一部分**：少一条前缀，开发时
 * 那条接口就直接 404（Vite 没有对应的 POST 路由），而生产由 Hub 同源托管
 * 照样能跑——于是这个缺陷只在 `mise run dev` 下存在。
 *
 * `/login` 与 `/setup` 正是这么漏掉的：它们看起来像页面，`POST` 那一半
 * 却是后端端点，且不在 `/api/*` 下。
 */
const proxy = devProxy

function entry(key: string): ProxyOptions {
  const raw = proxy[key]
  expect(raw, `dev 代理缺少 ${key}，开发时这条接口会 404`).toBeDefined()
  return typeof raw === 'string' ? { target: raw } : raw
}

type Bypass = (req: { method?: string; url?: string }) => unknown

function bypassOf(key: string): Bypass {
  const bypass = entry(key).bypass
  expect(bypass, `${key} 需要按方法分派：POST 给 Hub、GET 留给 SPA`).toBeTypeOf('function')
  return bypass as unknown as Bypass
}

describe('dev 代理', () => {
  it('后端路径全部转发到 Hub', () => {
    for (const key of ['/api', '/healthz', '/login', '/setup']) {
      expect(entry(key).target).toBe('http://127.0.0.1:8080')
    }
    expect(entry('/ws').target).toBe('ws://127.0.0.1:8080')
    expect(entry('/ws').ws).toBe(true)
  })

  it('登录端点走 Hub，登录页面留在 Vite', () => {
    const bypass = bypassOf('/login')
    expect(bypass({ method: 'POST', url: '/login' })).toBeUndefined()
    expect(bypass({ method: 'GET', url: '/login' })).toBe('/login')
  })

  it('初始化端点走 Hub，初始化页面留在 Vite', () => {
    const bypass = bypassOf('/setup')
    expect(bypass({ method: 'POST', url: '/setup' })).toBeUndefined()
    expect(bypass({ method: 'GET', url: '/setup' })).toBe('/setup')
    expect(bypass({ method: 'HEAD', url: '/setup' })).toBe('/setup')
  })
})
