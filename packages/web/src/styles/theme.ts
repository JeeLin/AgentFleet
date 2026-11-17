/* ===========================================================================
   主题切换机制 —— 逐条对齐 `prototype/assets/swatch.js`
   ---------------------------------------------------------------------------
   四套配色以「同一组 `--p-*` 变量、各一份 `:root`/`html[data-theme]`」存在
   （`theme-{neutral,warm,paper,cream}.css`），本模块只负责**决定该用哪套**并把
   结果落到 `<html>` 上。颜色本身一行都不在这里。

   为什么不用「按需 append 一个 <link>」（原型的做法）：原型是静态 HTML，四份
   主题文件在磁盘上按需换 `href`；这里是打包产物，四份都在 bundle 里，用
   `html[data-theme="..."]` 选择器切换更省一次网络往返，且**不会闪**（四套样式
   首屏就在，切换只改一个属性）。

   状态的优先序与原型完全一致（`swatch.js:69-76`）：
     ① URL `?theme=<id>`（可分享、换页不丢）
     ② 「跟随系统」开启 → 按 `prefers-color-scheme` 在**保留用户冷暖**的前提下挑
     ③ localStorage `af-theme`
     ④ 默认 `warm`（乙 · 暖米灰，暗色暖调）

    ── 为什么默认是 `warm` 而不是原型 `index.html:8` 引的 `neutral` ──────
    **这是用户裁定，不是照抄原型**（2026-11-16：「默认改为 warm 吧」）。
    原型首页引 `theme-neutral.css` 只说明「演示时先看这套」，不代表产品默认；
    深色暖调蓝光更少，是长时间盯着的控制台更合适的起点。
    `theme-*.css` 四份文件的内容仍逐值照搬原型，只有「先选哪一套」不同。

   设置页的外观区调 `applyTheme` / `followOS` / `setFollowOS` / `currentTheme`。
   =========================================================================== */

export type ThemeId = 'neutral' | 'warm' | 'paper' | 'cream'

interface ThemeMeta {
  id: ThemeId
  /** 原型 `swatch.js:32-35` 的四条：甲/乙 暗，丙/丁 亮；冷暖各半。 */
  mode: '暗' | '亮'
  tone: '冷' | '暖'
}

export const THEMES: readonly ThemeMeta[] = [
  { id: 'neutral', mode: '暗', tone: '冷' },
  { id: 'warm', mode: '暗', tone: '暖' },
  { id: 'paper', mode: '亮', tone: '冷' },
  { id: 'cream', mode: '亮', tone: '暖' },
]

/**
 * 无任何偏好时的默认主题：乙 · 暖米灰（暗色暖调）。
 *
 * **用户裁定 2026-11-16**（上一版是甲 · 中性冷灰）。理由见文件头注释：
 * 原型首页先引 `theme-neutral.css` 只是演示顺序，不构成产品默认的依据。
 */
export const DEFAULT_THEME: ThemeId = 'warm'

const KEY = 'af-theme'
const OS_KEY = 'af-theme-follow-os'

/* localStorage 在隐私模式、被策略禁用、或 file:// 直接打开时都可能抛。
   一旦抛出来整个模块就挂了，后面的切换根本不生效——所以每次访问都包起来，
   存不住就算了，不能让功能连带失效（原型 `swatch.js:43-51` 同样的理由）。 */
function safeGet(key: string): string | null {
  try {
    return globalThis.localStorage?.getItem(key) ?? null
  } catch {
    return null
  }
}

function safeSet(key: string, val: string): void {
  try {
    globalThis.localStorage?.setItem(key, val)
  } catch {
    /* 存不住不影响当前页 */
  }
}

function isThemeId(v: unknown): v is ThemeId {
  return typeof v === 'string' && THEMES.some((t) => t.id === v)
}

function savedTheme(): ThemeId | null {
  const saved = safeGet(KEY)
  return isThemeId(saved) ? saved : null
}

export function followOS(): boolean {
  return safeGet(OS_KEY) === '1'
}

/* 跟随系统时：暗色环境给甲/乙，亮色环境给丙/丁。
   保留用户上次选的那一侧的「冷暖」，只换明暗——否则白天打开是纸白、晚上
   突然变成暖米灰，冷暖也跟着跳（原型 `swatch.js:57-67`）。

   **用户从未选过时按默认主题的冷暖走**（`DEFAULT_THEME` = `warm` → 暖），
   而不是写死 `'冷'`：否则「开了跟随系统的新用户」在暗色环境得到 neutral（冷）、
   而「没开跟随系统的新用户」得到 warm（暖），同一个产品里存在两套不同的
   「无偏好默认」。
   注意**不能**写成 `?? ambiguous` 之类——`savedTheme()` 为 null 时必须有确定的
   一侧，这里取 `DEFAULT_THEME` 自身的 tone，让默认值只有一个定义处。 */
function themeFromOS(): ThemeId {
  const dark = globalThis.matchMedia?.('(prefers-color-scheme: dark)')?.matches ?? true
  const saved = savedTheme()
  const fallbackTone =
    THEMES.find((t) => t.id === DEFAULT_THEME)?.tone ?? THEMES[0]?.tone ?? '冷'
  const tone = THEMES.find((t) => t.id === saved)?.tone ?? fallbackTone
  const want = dark ? '暗' : '亮'
  const hit = THEMES.find((t) => t.mode === want && t.tone === tone)
  return hit?.id ?? THEMES.find((t) => t.mode === want)?.id ?? DEFAULT_THEME
}

function themeFromUrl(): ThemeId | null {
  try {
    const q = new URLSearchParams(globalThis.location?.search ?? '').get('theme')
    return isThemeId(q) ? q : null
  } catch {
    return null
  }
}

/** 当前该用哪套。设置页挂载时读它来初始化选中态。 */
export function currentTheme(): ThemeId {
  return themeFromUrl() ?? (followOS() ? themeFromOS() : (savedTheme() ?? DEFAULT_THEME))
}

/** 把一套主题落到 `<html data-theme data-theme-mode>` 上。 */
export function applyTheme(id: ThemeId, persist = false): ThemeId {
  // 兜底必须走 `DEFAULT_THEME` 而不是 `THEMES[0]`：后者是「清单第一项」，
  // 与「默认主题」在默认改成 warm 之后已经不是同一个东西了。取 `THEMES[0]`
  // 会让「传了非法 id」静默落到 neutral，而「什么都没设」落到 warm——同一个
  // 产品里两套「默认」，且改默认值时只改一处不会跟着变。
  const theme = THEMES.find((t) => t.id === id) ?? THEMES.find((t) => t.id === DEFAULT_THEME)!
  const root = globalThis.document?.documentElement
  if (root) {
    root.dataset.theme = theme.id
    root.dataset.themeMode = theme.mode === '暗' ? 'dark' : 'light'
  }
  if (persist) safeSet(KEY, theme.id)
  return theme.id
}

/** 手动选一套 = 不再跟随系统；同时把 URL 换成这一版，复制地址就是同一套。 */
export function setTheme(id: ThemeId): ThemeId {
  const applied = applyTheme(id, true)
  if (followOS()) setFollowOS(false)
  try {
    const url = new URL(globalThis.location.href)
    url.searchParams.set('theme', applied)
    globalThis.history?.replaceState(null, '', url)
  } catch {
    /* 地址栏改不了不影响切换 */
  }
  return applied
}

export function setFollowOS(on: boolean): void {
  safeSet(OS_KEY, on ? '1' : '0')
}

/** 「跟随系统」开关。开 → 立刻按系统重算；关 → 回到用户上次手选的那套。 */
export function toggleFollowOS(): { follow: boolean; theme: ThemeId } {
  const on = !followOS()
  setFollowOS(on)
  const theme = applyTheme(on ? themeFromOS() : (savedTheme() ?? DEFAULT_THEME), on ? false : true)
  return { follow: on, theme }
}

/* 首屏尽早应用，避免先闪一下默认色再变（原型 `swatch.js:96`）。
   放在模块顶层而不是 `onMounted`：模块被 `main.ts` import 时立刻执行。 */
if (typeof document !== 'undefined') {
  applyTheme(currentTheme(), false)

  // 系统明暗变了，跟着换（只在跟随系统时）
  globalThis.matchMedia?.('(prefers-color-scheme: dark)')?.addEventListener?.('change', () => {
    if (followOS()) applyTheme(themeFromOS(), false)
  })
}
