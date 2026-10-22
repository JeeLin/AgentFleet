/* ===========================================================================
   SFC 样式规则的读取助手
   ---------------------------------------------------------------------------
   为什么需要它：vitest 默认不处理 CSS（`css: false`），happy-dom 也不做
   排版，于是 `getComputedStyle(el).pointerEvents` 在测试里恒为 `''`。
   「样式里写了这条规则」这件事只能从**样式源**上读。

   于是有一条硬规矩：**凡是「样式必须这么写」的断言，都不许退化成
   「样式表有若干条规则」这种恒真断言**——那是把没测的东西写成测过了。
   =========================================================================== */

import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../../..')

/** 读一个仓库内文件（相对仓库根）。 */
export function readRepoFile(relative: string): string {
  return readFileSync(resolve(REPO_ROOT, relative), 'utf-8')
}

/**
 * 取一个 SFC 里某条 CSS 规则的声明体（`{ ... }` 里的原文）。
 *
 * 只认**顶层**规则：SFC 的 `<style>` 里没有嵌套语法，所以「选择器后面
 * 紧跟 `{`」就是一条规则的开始。不在表里返回空串——调用方据此断言
 * 「这条规则必须存在」，缺了就红。
 */
export function cssRuleOf(source: string, selector: string): string {
  const at = source.indexOf(`${selector} {`)
  if (at < 0) return ''
  const open = at + selector.length + 1
  const close = source.indexOf('}', open)
  if (close < 0) return ''
  return source.slice(open, close)
}
