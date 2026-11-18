/* ===========================================================================
   `GitDiff.vue` 空态的三态：`stale` / `truncated` / 普通空
   ---------------------------------------------------------------------------
   `GitDiff.vue:74-80` 有三条**用户可见**的说明，各自说的是不同的事：

     `stale`        「正在读取这一份 diff…」        —— 还没结论，请等
     `truncated`    「改动太长了，设备侧一截就什么都不剩。」—— 有结论，但结论是「取不到」
     其余           「这个文件没有可显示的改动——它可能是未跟踪的空文件，
                     或改动只涉及模式 / 权限。」    —— 有结论，就是这个文件没变

   把三者压成一句是**在撒谎**：第二条那半句注释（`GitDiff.vue:76-77`）写着
   「`truncate_utf8` 在首字节非法 UTF-8 时能截到什么都不剩，这时说『没有可显示
   的改动』是在撒谎」。第三条也被逐字论证过（「可能是未跟踪的空文件，或改动只
   涉及模式 / 权限」解释了「为什么 git 说改了而这里看不到」）。

   ── 为什么直接挂组件，而不是再走一遍 SessionPage
   `SessionPage.test.ts` 已经覆盖了**集成**路径（`:2351` 的 stale、`:2052` 的
   truncated 横幅），但两处都只断言了**片段**（`toContain('截断')` /
   `toContain('正在读取这一份 diff')`），并且第三条（普通空态）没有任何断言。
   2026-11 第3轮测试验证 P17 实测：把第三条整句换成「没有改动。」→
   **整套用例照样全绿**。

   集成路径还掺着通道、请求、分段切换，一条文案要红得先穿过七八个中间态；
   这个组件「不认识通道，也不发请求」（`GitDiff.vue:13-14` 的自我纪律），
   所以直接给它 props 就是最短的判据路径。两种都留：集成保「接得上」，
   这里保「说的是哪一句」。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import GitDiff from '@/domains/session/GitDiff.vue'
import { mountIn } from '@/test/mount'

/** 挂一份 diff，取空态那一段的文本。 */
function noteOf(props: {
  unified?: string
  truncated?: boolean
  path?: string | null
  stale?: boolean
}): { empty: HTMLElement | null; trunc: HTMLElement | null; text: string } {
  const w = mountIn(GitDiff, {
    props: {
      unified: '',
      truncated: false,
      path: 'src/a.rs',
      stale: false,
      ...props,
    },
  })
  const empty = w.element.querySelector<HTMLElement>('[data-git-diff-empty]')
  const trunc = w.element.querySelector<HTMLElement>('[data-git-diff-truncated]')
  return { empty, trunc, text: w.element.textContent ?? '' }
}

describe('GitDiff：空态三态各说各的，不许压成一句', () => {
  it('普通空态：说「没有可显示的改动」，并把「为什么可能看不到」讲出来', () => {
    // 这一句是 P17 的靶子：换成「没有改动。」时整套用例照样绿。
    const { empty, text } = noteOf({ unified: '' })

    expect(empty, '空正文要命中 data-git-diff-empty 那一段').not.toBeNull()
    expect(empty?.textContent?.trim()).toBe(
      '这个文件没有可显示的改动——它可能是未跟踪的空文件，或改动只涉及模式 / 权限。',
    )
    // 反向：普通空态不该同时报「截断」
    expect(text).not.toContain('改动太长了')
  })

  it('truncated：说「一截就什么都不剩」，不说「没有可显示的改动」', () => {
    // `GitDiff.vue:76-77`：空正文 + 已截断 ≠「没有改动」。
    const { empty, text } = noteOf({ unified: '', truncated: true })

    expect(empty, 'truncated 也要落在同一段空态里（只是换一句）').not.toBeNull()
    expect(empty?.textContent?.trim()).toBe('改动太长了，设备侧一截就什么都不剩。')
    // 两句必须分得开：把这一支删掉后模板会落到普通空态，就是一次撒谎。
    expect(text, '截断时不许说「没有可显示的改动」').not.toContain('没有可显示的改动')
  })

  it('truncated 且正文非空：正文照常渲染，另有一条「下面这些不是全部」的横幅', () => {
    // `GitDiff.vue:89-94`：截断提示**放在 `lines.length` 判定之外**。
    // 早先它在 `<template v-else>` 里，于是「有内容但被截断」时提示语不出现，
    // 用户以为「就改了这些」。
    const { empty, trunc, text } = noteOf({
      unified: '@@ -1 +1 @@\n-old\n+new\n',
      truncated: true,
    })

    expect(empty, '有正文时不该出现空态').toBeNull()
    expect(trunc, '有正文时的截断要显式说出来').not.toBeNull()
    expect(trunc?.textContent).toContain('下面这些不是全部改动')
    expect(text).toContain('+new')
  })

  it('stale：说「正在读取」，不给出任何结论', () => {
    // `GitDiff.vue:74` 是最外层 `v-if`：`stale` 时必须盖掉后面两态——
    // stale 时 `lines` 被清空（`:35-36`），所以若不先判 stale，这一段会
    // 渲染成「没有可显示的改动」，把「还在读」说成「没有」。
    const { empty, text } = noteOf({ unified: 'FROM_A\n', stale: true })

    expect(text).toContain('正在读取这一份 diff')
    expect(empty, 'stale 时不该落到空态那一段').toBeNull()
    expect(text, 'stale 时不许下结论').not.toContain('没有可显示的改动')
    expect(text, 'stale 时不许展示上一份的正文').not.toContain('FROM_A')
  })
})
