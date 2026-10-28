import { readFileSync, readdirSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { describe, expect, it } from 'vitest'

/**
 * `trajectory_event` 那一行形状的**重复声明**门禁。
 *
 * 为什么数而不是 `toContain`：`toContain` 看不见多出来的第二份——而这条缺陷
 * 的形态恰恰是「同一个形状在 N 个文件里各写一遍，其中一份少了个字段」。
 * 组件里再写一份「与 `EventOut` 同形」的接口时，编译器不会说话（少字段的
 * 接口照样接得住多字段的对象），所以只有**数**能看见它。
 */
const SRC = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const REPO_ROOT = resolve(SRC, '../../..')

/** 仓库内所有 `.ts` / `.vue` 源文件（不含本测试自己）。 */
function sourceFiles(): string[] {
  const out: string[] = []
  const walk = (dir: string): void => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const at = resolve(dir, entry.name)
      if (entry.isDirectory()) walk(at)
      else if (/\.(ts|vue)$/.test(entry.name) && !entry.name.endsWith('.test.ts')) out.push(at)
    }
  }
  walk(SRC)
  return out
}

/**
 * 后端 `EventOut` 的字段名。
 *
 * 从 Rust 源码里读而不是在测试里再列一遍：后者会让「前端加了个字段」与
 * 「后端加了个字段」两边各自悄悄变，谁也管不着谁。
 */
function backendEventOutFields(): string[] {
  const rs = readFileSync(
    resolve(REPO_ROOT, 'crates/hub-server/src/routes/conversation.rs'),
    'utf-8',
  )
  const body = /pub struct EventOut \{([\s\S]*?)\n\}/.exec(rs)?.[1]
  expect(body, 'conversation.rs 里找不到 `pub struct EventOut`').toBeTruthy()
  return [...(body as string).matchAll(/^\s*pub ([a-z_]+):/gm)].map((m) => m[1] as string)
}

/** 前端 `TrajectoryEvent` 的字段名。 */
function frontendFields(): string[] {
  const ts = readFileSync(resolve(SRC, 'lib/api.ts'), 'utf-8')
  const body = /export interface TrajectoryEvent \{([\s\S]*?)\n\}/.exec(ts)?.[1]
  expect(body, 'api.ts 里找不到 `export interface TrajectoryEvent`').toBeTruthy()
  return [...(body as string).matchAll(/^\s{2}(?!\/|\*)([a-z_]+)\??:/gm)].map((m) => m[1] as string)
}

describe('trajectory_event 的行形状只声明一次', () => {
  it('全 src 里只有 api.ts 一处声明这个形状（组件不许再抄一份）', () => {
    /**
     * 「声明」= 一个自己写出 `payload:`（这个行形状独有的字段）的接口。
     * 借 `import type` 用它的文件不写这一行，所以数不到它头上。
     *
     * **必须两种写法都认**：`^\s*payload:` 只抓得住逐字段换行的接口，写成
     * 一行（`{ id: string; payload: string }`）的副本会从这条缝里漏过去——
     * 那样「只声明一次」就成了「只声明一次，只要你别写在一行里」。下面那条
     * 断言同时认 `payload: string`（换行写法）与 `payload:` 出现在同一行
     * 接口体里的情况，用法：`;` 或 `,` 分隔的单行接口。
     */
    const declarers = sourceFiles().filter((f) => {
      const src = readFileSync(f, 'utf-8')
      // 逐字段换行：`payload: string` 自己占一行
      if (/^\s*payload\s*\??:\s*string\b/m.test(src)) return true
      // 单行接口：`{ ...; payload: string; ... }` / `{ ..., payload: string, ... }`
      return /[{;,\s]payload\s*\??:\s*string\b/.test(src)
    })
    expect(declarers.map((f) => f.slice(SRC.length + 1)).sort(), '这些文件各自又声明了一遍').toEqual([
      'lib/api.ts',
    ])
  })

  it('`duration_ms` / `duration_source` 在前端只出现在这一个声明里（零额外副本）', () => {
    // 数的是**类型声明**而非使用点：测试夹具里的字面量不算一份声明，
    // 而组件里再写一份接口必然同时写出这两个字段名之一。
    //
    // **这条只管「别抄第二份」，管不了「声明了但没人画」**：`duration_ms`
    // 曾经正是这样——类型里有、字段集门禁也过，而三个视图谁都没渲染它，
    // 于是「轨迹可回放」少了耗时一项却全绿。那种缺口由渲染侧的用例守
    // （`components/TrajDrawer.test.ts` 的「每行画出耗时」）。
    const declarers = sourceFiles().filter((f) =>
      /^\s*(duration_ms|duration_source)\s*\??:/m.test(readFileSync(f, 'utf-8')),
    )
    expect(declarers.map((f) => f.slice(SRC.length + 1)).sort()).toEqual(['lib/api.ts'])
  })

  it('前端字段集与后端 EventOut 逐字段同名（后端加了列，前端立刻红）', () => {
    expect(backendEventOutFields().length).toBeGreaterThan(0)
    expect([...frontendFields()].sort()).toEqual([...backendEventOutFields()].sort())
  })
})
