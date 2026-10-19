/* ===========================================================================
   split.ts —— 分栏宽度的纯计算（规范 §3.15 / §7.1）
   ---------------------------------------------------------------------------
   把手能拖、能用键盘调，但**能调到哪儿**是一个纯函数：30–70%。
   把它抽出来是因为边界条件（拖过头、容器还没量到宽度、键盘按到头）
   都是「算错一位」的形态——写在组件的事件处理里，测试要连 DOM 一起摆。

   ── 为什么下限是 30 而不是「最小像素」 ──────────────────────────────
   像素下限随窗口宽度变化，同一个 320px 在 1440 屏上是 22%、在 1024 屏上
   是 31%——于是「把面板拖窄」在不同屏幕上得到不同的会话区宽度。
   规范 §7.1 给的是百分比（默认 56%），这里也按百分比收口。
   =========================================================================== */

/** 对话区占工作台正文的最小 / 最大百分比。 */
export const SPLIT_MIN = 30
export const SPLIT_MAX = 70

/** 方向键一次的步长（§3.15：`←→` 调 4%）。 */
export const SPLIT_STEP = 4

/** 默认宽度（§7.1：对话区默认 56%）。 */
export const SPLIT_DEFAULT = 56

/**
 * 收进 30–70。
 *
 * `NaN` 回落到默认值（它表示「这个值没算出来」，写进样式会让整条分栏塌掉）；
 * `±Infinity` 仍按大小收口（一个拖到屏幕外的位置就是「拖到头」）。
 */
export function clampSplit(value: number): number {
  if (Number.isNaN(value)) return SPLIT_DEFAULT
  if (value < SPLIT_MIN) return SPLIT_MIN
  if (value > SPLIT_MAX) return SPLIT_MAX
  return value
}

/** 键盘调节。`Home` / `End` 直接给两端，不经过步长。 */
export function stepSplit(
  value: number,
  key: 'left' | 'right' | 'home' | 'end',
  step = SPLIT_STEP,
): number {
  switch (key) {
    case 'home':
      return SPLIT_MIN
    case 'end':
      return SPLIT_MAX
    case 'left':
      return clampSplit(value - step)
    case 'right':
      return clampSplit(value + step)
  }
}

/**
 * 拖动后的宽度。
 *
 * `trackPx` 是分栏容器的宽度。0 或负值表示「还没量到」（首次渲染、隐藏中、
 * 测试环境的 happy-dom）——此时**不移动**而不是算出 `Infinity`：
 * 一个 NaN 宽度会让整条分栏塌掉，而「拖不动」至少是能看出来的。
 */
export function splitFromDrag(
  startPercent: number,
  deltaPx: number,
  trackPx: number,
): number {
  if (!Number.isFinite(trackPx) || trackPx <= 0) return clampSplit(startPercent)
  return clampSplit(startPercent + (deltaPx / trackPx) * 100)
}
