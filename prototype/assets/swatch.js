/* ===========================================================================
   swatch.js —— 主题切换（四套配色）
   ---------------------------------------------------------------------------
     甲 中性冷灰   暗 · 冷   默认
     乙 暖米灰     暗 · 暖
     丙 纸白偏冷   亮 · 冷
     丁 米白偏暖   亮 · 暖

   入口只有一个：设置页 →「外观」。主题是产品功能，不是调试开关，
   所以不再在页面右上角挂浮层按钮 —— 那会让原型工具和产品功能混在一起。

   为什么做成脚本而不是复制四套页面：四套页面一旦分开就会各自漂移，
   你比较的就不再是「颜色」，而是「四份不同的 HTML」。

   状态存在 URL 的 ?theme= 里，所以：
   - 换页不丢（shell.js 会把这个参数带过去）
   - 可以直接把某一版的链接发给别人看
   - 不依赖 localStorage（隐私模式下也能用）
   =========================================================================== */

/* ---------------------------------------------------------------------------
   整段包在立即执行函数里。
   原因：shell.js 也在全局声明了 ROOT，两个 <script> 共用同一全局作用域时，
   后加载的那个会直接抛 SyntaxError: Identifier 'ROOT' has already been declared，
   而且是一行都不执行、连报错都在控制台里不显眼。
   =========================================================================== */

(function () {
'use strict';

  const THEMES = [
    { id: 'neutral', name: '甲 · 中性冷灰', mode: '暗', tone: '冷', hint: '正文 9.27:1' },
    { id: 'warm',    name: '乙 · 暖米灰',   mode: '暗', tone: '暖', hint: '正文 9.16:1 · 蓝光更少' },
    { id: 'paper',   name: '丙 · 纸白偏冷', mode: '亮', tone: '冷', hint: '正文 12.79:1' },
    { id: 'cream',   name: '丁 · 米白偏暖', mode: '亮', tone: '暖', hint: '正文 12.38:1 · 蓝光更少' },
  ];

  const DEFAULT = 'neutral';          /* 默认甲 · 中性冷灰 */
  const ROOT = location.pathname.includes('/panels/') ? '../' : '';
  const KEY = 'af-theme';
  const OS_KEY = 'af-theme-follow-os';

  /* localStorage 在隐私模式、被策略禁用、或 file:// 直接打开时都可能抛异常。
     一旦它抛出来，整个脚本就挂了，后面的切换根本不生效 ——
     所以每次访问都要包起来，存不住就算了，不能让功能连带失效。 */
  function safeGet(key) {
    try { return localStorage.getItem(key); } catch (e) { return null; }
  }
  function safeSet(key, val) {
    try { localStorage.setItem(key, val); } catch (e) { /* 存不住不影响当前页 */ }
  }

  function followOS() {
    return safeGet(OS_KEY) === '1';
  }

  /* 跟随系统时：暗色环境给甲/乙，亮色环境给丙/丁。
     保留用户上次选的那一侧的「冷暖」，只换明暗 ——
     否则白天打开是纸白，晚上突然变成暖米灰，冷暖也跟着跳。 */
  function themeFromOS() {
    const dark = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
    const saved = THEMES.find((t) => t.id === safeGet(KEY));
    const tone = saved ? saved.tone : '冷';
    const want = dark ? '暗' : '亮';
    const hit = THEMES.find((t) => t.mode === want && t.tone === tone);
    return (hit || THEMES.find((t) => t.mode === want) || THEMES[0]).id;
  }

  function currentTheme() {
    const q = new URLSearchParams(location.search).get('theme');
    if (q && THEMES.some((t) => t.id === q)) return q;
    if (followOS()) return themeFromOS();
    const saved = safeGet(KEY);
    if (saved && THEMES.some((t) => t.id === saved)) return saved;
    return DEFAULT;
  }

  function applyTheme(id, persist) {
    const t = THEMES.find((x) => x.id === id) || THEMES[0];
    /* 主题文件永远带 ?v= 版本号，否则浏览器会用缓存的旧配色 */
    const link = document.getElementById('af-theme');
    if (link) link.href = `${ROOT}styles/theme-${t.id}.css?v=6`;
    document.documentElement.dataset.theme = t.id;
    document.documentElement.dataset.themeMode = t.mode;
    if (persist !== false) safeSet(KEY, t.id);

    /* 同步设置页里那四个选项的选中态 */
    document.querySelectorAll('[data-theme-pick]').forEach((el) => {
      const on = el.dataset.themePick === t.id;
      el.setAttribute('aria-checked', String(on));
    });
    return t;
  }

  /* 尽早应用，避免先闪一下默认色再变 */
  applyTheme(currentTheme(), false);

  document.addEventListener('DOMContentLoaded', () => {
    applyTheme(currentTheme(), false);

    /* 设置页里的四张配色卡 */
    document.querySelectorAll('[data-theme-pick]').forEach((el) => {
      el.addEventListener('click', () => {
        const id = el.dataset.themePick;
        applyTheme(id);
        /* 手动选了一套，就等于不再跟随系统 */
        if (followOS()) {
          safeSet(OS_KEY, '0');
          const sw = document.getElementById('af-follow-os');
          if (sw) sw.setAttribute('aria-checked', 'false');
        }
        /* 把 URL 也换掉，这样复制地址得到的是同一版。
           file:// 打开时 replaceState 可能抛异常，所以包起来。 */
        try {
          const url = new URL(location.href);
          url.searchParams.set('theme', id);
          history.replaceState(null, '', url);
        } catch (e) { /* 地址栏改不了不影响切换 */ }
      });
    });

    /* 「跟随系统的明暗设置」开关 */
    const sw = document.getElementById('af-follow-os');
    if (sw) {
      sw.setAttribute('aria-checked', String(followOS()));
      sw.addEventListener('click', () => {
        const on = sw.getAttribute('aria-checked') !== 'true';
        sw.setAttribute('aria-checked', String(on));
        safeSet(OS_KEY, on ? '1' : '0');
        applyTheme(on ? themeFromOS() : (safeGet(KEY) || DEFAULT));
      });
    }

    /* 系统明暗变了，跟着换 */
    if (window.matchMedia) {
      const mq = window.matchMedia('(prefers-color-scheme: dark)');
      const onChange = () => { if (followOS()) applyTheme(themeFromOS(), false); };
      if (mq.addEventListener) mq.addEventListener('change', onChange);
      else if (mq.addListener) mq.addListener(onChange);
    }
  });
})();
