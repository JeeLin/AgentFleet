/* ===========================================================================
   「建会话为什么没建成」的九句用户可读理由。
   ---------------------------------------------------------------------------
   被测的是 `api.ts` 的 `createSessionErrorLabel(code)`——`NewSessionPage.vue:209`
   唯一的失败出口（`err instanceof ApiError ? createSessionErrorLabel(err.code)
   : '建会话没有成功：Hub 没有回应。'`）。用户看到的那一句话**完全**来自这里。

   为什么单独一个文件（而不是塞进 `api.test.ts`）：`api.test.ts` 守的是数据
   形状（`trajectory_event` 的重复声明），是一条**跨文件**门禁；这里守的是
   文案与分支覆盖，两件事的读者不同。同目录按被测面拆文件已有先例
   （`NewSessionPage.test.ts` / `api.test.ts`）。

   ── 为什么每条 case 都要单独断言（2026-11 第3轮测试验证 P24 / P25）
   `api.ts` 的 9 条 `case` 里有 7 条从没被任何用例走到（`src` 行覆盖 81.25%，
   未覆盖行 189-190, 203, 205, 207, 209, 211, 213, 215）。实测把
   `agent_required`（`api.ts:213`）与 `agent_unavailable`（`:211`）的整句换成
   `'建会话失败了。'`，**整套用例照样全绿**。

   而这两句恰恰是「用户唯一能看到的为什么建不出来」，`api.ts:192-197` 还专门
   写了注释论证措辞（「只描述事实与下一步，不写『操作失败，请重试』这类把判断
   还给用户的句子」）。所以这里逐条钉住**整个字符串**，不是 `toContain('agent')`
   之类的片段——片段断言挡不住「整句被换成一句万金油」。

   ── 两种断言各管一件事
   ① `toEqual` 整句：措辞不许漂。这也是「分支命中」的证明——换到 `default`
      会得到 `建会话没有成功（agent_required）。`，与期望句不等。
   ② 唯一性：**九条 case + default 的文案两两不等**。只钉 ① 的话，
      「把 A 的 case 整段删掉、让 A 落到 default」会红（好），但「把 A 的 case
      删掉、同时把 B 的文案抄给 default」这种更隐蔽的合并就拦不住了；
      而规范禁止的正是「把不同的下一步压成同一句话」（`api.ts:192-197`、
      `auth.ts` 文件头 §4.7）。
   =========================================================================== */

import { describe, expect, it } from 'vitest'

import { createSessionErrorLabel } from '@/domains/session/api'

/**
 * 九条 `case` 的 code → 用户看到的那一整句。
 *
 * 顺序与 `api.ts` 的 `switch` 一致。这里**故意**把文案抄一份而不是从源码里
 * 抽：抄的那份是「契约」，源码是「实现」，两者漂了就该红。从源码里抽的话
 * 无论源码写什么都会自洽，等于没断言。
 */
const LABELS: ReadonlyArray<[code: string, label: string]> = [
  [
    'device_offline',
    '这台设备现在离线，Hub 连不上它。等它重新上线后再建会话。',
  ],
  ['invalid_workspace_path', '路径要写成绝对路径：以 / 或 ~ 开头。'],
  ['not_found', '这台设备不存在了。刷新设备列表后再试。'],
  ['session_expired', '登录已失效，请重新登录。'],
  ['invalid_request_body', '提交的内容不完整：设备、路径都要填。'],
  [
    'agent_unavailable',
    '这台设备上这个 agent 现在跑不起来（装了但没有可用的 ACP 入口）。换一个 agent，或先在这台设备上装好它。',
  ],
  [
    'agent_required',
    '建会话要先选一个 agent：没有 agent 的会话打不开对话。',
  ],
]

describe('createSessionErrorLabel：九条理由逐条钉住', () => {
  /*
    这条前置不是形式：下面 `it.each` 遍历 LABELS，表被清空时它会「零个用例
    全绿」——那是恒真断言之外最难看见的一种假绿。
  */
  it('表里登记了 7 条已知 code（少一条就说明有人删了下面的契约）', () => {
    expect(LABELS.length).toBe(7)
  })

  it.each(LABELS)('%s 的文案原样呈现给用户', (code, label) => {
    expect(createSessionErrorLabel(code), `${code} 的用户可读理由`).toBe(label)
  })

  it('每条文案都点名了「下一步」，不是一句万金油', () => {
    // 「建会话失败了。」这类改写**不含任何一个**下列词：它就消失在这条断言里。
    // 判据故意宽松（有其一即可）：要守的是「还留了一条线索」，不是逐句文风。
    const actionable = ['再', '换', '先', '刷新', '等', '重', '要', '开头', '装']
    for (const [code, label] of LABELS) {
      expect(
        actionable.some((w) => label.includes(w)),
        `${code} 的文案里找不到任何「下一步」线索：${label}`,
      ).toBe(true)
    }
  })

  it('未知 code 把 code 本身带出来（前端落后于 Hub 时露出真名，不编一句好话）', () => {
    // `api.ts:196`：「未知 code 原样带出：前端落后于 Hub 时，露出那个 code
    // 比编一句好话更有用」。这条同时证明**没有**一条已知 code 落进 default
    // ——default 的形状与它们全都不同。
    expect(createSessionErrorLabel('some_future_code')).toBe(
      '建会话没有成功（some_future_code）。',
    )
  })

  it('九条文案两两不等（不同的下一步不许被压成同一句话）', () => {
    const all = [...LABELS.map(([, label]) => label), createSessionErrorLabel('nope')]
    // 逐对比较并报出撞车的那一对：`new Set(...).size` 只说「撞了」，
    // 不说是谁撞谁，排查时还得再猜。
    for (let i = 0; i < all.length; i++) {
      for (let j = i + 1; j < all.length; j++) {
        expect(all[i], `第 ${i} 条与第 ${j} 条的文案撞车了`).not.toBe(all[j])
      }
    }
  })
})
