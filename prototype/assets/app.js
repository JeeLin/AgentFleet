/* ===========================================================================
   app.js —— 外壳注入 + mock 数据 + 页面装配（原型专用）
   ---------------------------------------------------------------------------
   分工：keynav.js 管「焦点归谁、哪个键干什么」；这个文件管「外壳长什么样、
   数据从哪来、页面拿到外壳之后补什么」。**页面逻辑不在 HTML 里**——
   每个页面的 <script> 只声明数据与结构，行为一律到这里来（AF.mount）。

   ── 为什么外壳由 JS 注入 ────────────────────────────────────────────
   12 个页面各写一份 <nav>，任何一次导航改动都要改 12 处，必然会漂。
   注入的代价是「没 JS 就没有壳」，而原型本来就跑在 JS 里（mock、模拟、
   焦点栈全靠它），这个代价为零。

   上一版的问题不在注入本身，在**时机**：keynav.js 与 app.js 都挂
   DOMContentLoaded，谁先跑取决于 <script> 顺序。这一版注入是同步的——
   <script> 放在 body 末尾，执行时 DOM 已就绪，注入完立刻挂页面，
   没有竞态。DOMContentLoaded 只用来做首焦点与自检播报。
   =========================================================================== */
(function () {
  'use strict';

  var AF = (window.AF = window.AF || {});

  /* =========================================================================
     §1 导航（IA 的唯一真相源）
     -------------------------------------------------------------------------
     四条一级目的地 + 认证流程。**动作不算目的地**：「新建会话」是
     sessions 页上的一个按钮，不是导航项——导航里放动作会让用户以为
     它是一个地方，于是收藏、后退、直接输 URL 全都变得语义可疑。

     已删路由（规范 §1.3 否决项）：/w/:node/:path（工作区）、/workspace、
     /tasks（任务分发）、/artifacts、/devices/:id。删掉的不是「暂缓」，
     是架构上不存在或本阶段不做，所以导航里也不出现。
     ======================================================================= */
  var NAV = [
    { group: '会话', items: [
      { href: 'sessions.html', label: '会话', icon: '◈', key: 'g s', cur: 'sessions' },
      { href: 'new-session.html', label: '新建会话', icon: '✚', key: 'g n', cur: 'new-session' }
    ]},
    { group: '环境', items: [
      { href: 'devices.html', label: '设备', icon: '▣', key: 'g d', cur: 'devices' },
      { href: 'agents.html', label: 'Agent 目录', icon: '☰', key: 'g a', cur: 'agents' }
    ]},
    { group: '系统', items: [
      { href: 'settings.html', label: '设置', icon: '⚙', key: 'g ,', cur: 'settings' }
    ]}
  ];

  var AUTH_NAV = [
    { href: 'login.html', label: '登录', icon: '⏎', cur: 'login' },
    { href: 'setup.html', label: '首次 setup', icon: '⚡', cur: 'setup' }
  ];

  /* =========================================================================
     §2 外壳模板
     -------------------------------------------------------------------------
     <main tabindex="-1">：跳转后的焦点落点（规范 §4.2）。
     data-page：页面给自己贴的当前路由名，注入时用来点导航项。
     ======================================================================= */
  function shellHTML(page, opts) {
    opts = opts || {};
    var nav = opts.auth ? AUTH_NAV : NAV;
    var groups = nav.map(function (g) {
      var items = g.items.map(function (it) {
        var cur = it.cur === page ? ' aria-current="page"' : '';
        return '<a class="af-sidebar__item" href="' + it.href + '"' + cur + '>' +
          '<span class="af-sidebar__icon" aria-hidden="true">' + it.icon + '</span>' +
          '<span class="af-sidebar__item-text">' + it.label + '</span>' +
          '<span class="af-sidebar__hint">' + it.key + '</span></a>';
      }).join('');
      return '<div class="af-navgroup"><div class="af-navgroup__title">' + g.group + '</div>' + items + '</div>';
    }).join('');

    return '' +
    '<div class="af-brand">' +
      '<span class="af-brand__mark" aria-hidden="true"></span>' +
      '<span class="af-brand__name">AGENTFLEET</span>' +
    '</div>' +
    '<nav class="af-sidebar" aria-label="主导航">' +
      groups +
      '<span class="af-spacer"></span>' +
      '<button class="af-sidebar__close" type="button" data-role="rail-collapse">' +
        '<span class="af-sidebar__icon" aria-hidden="true">⇤</span>' +
        '<span class="af-sidebar__item-text">折叠侧栏</span></button>' +
    '</nav>' +
    '<div class="af-topbar">' +
      '<button class="af-btn af-btn--ghost af-btn--sm af-btn--icon" type="button" ' +
        'data-role="rail-toggle" aria-label="折叠 / 展开侧栏" title="折叠侧栏（Ctrl+B）">' +
        '<span aria-hidden="true">⇥</span></button>' +
      (opts.title ? '<h1 class="af-topbar__title">' + esc(opts.title) + '</h1>' : '') +
      '<span class="af-spacer"></span>' +
      '<input class="af-input" type="search" placeholder="搜索…（/）" ' +
        'aria-label="搜索" data-role="search" style="width:200px">' +
      '<button class="af-btn af-btn--ghost af-btn--sm af-btn--icon af-tip" type="button" ' +
        'data-role="help-open" aria-label="键位帮助（?）" title="键位帮助（?）">' +
        '<span aria-hidden="true">?</span>' +
        '<span class="af-tip__body">键位帮助 <kbd>?</kbd></span></button>' +
    '</div>' +
    '<main class="af-main" tabindex="-1" data-role="main"></main>' +
    '<div class="af-statusbar">' +
      '<span class="af-statusbar__item"><span class="af-dot" style="color:var(--af-ok)"></span>Hub 已连接</span>' +
      '<span class="af-statusbar__item" data-role="status-conn">单用户 · 无其他会话</span>' +
      '<span class="af-spacer"></span>' +
      '<span class="af-statusbar__item af-statusbar__key"><kbd>?</kbd> 键位</span>' +
      '<span class="af-statusbar__item af-statusbar__key"><kbd>/</kbd> 搜索</span>' +
      '<span class="af-statusbar__item af-statusbar__key"><kbd>⌃B</kbd> 侧栏</span>' +
    '</div>';
  }

  /* `esc` 不在这里实现：真相源是 keynav.js 的 `AF.esc`（它渲染 `?` 面板，
     所以「页面里的转义」与「help 表里的转义」必须是同一个函数）。
     早先这里另写了一份逐字相同的实现，于是两处会各自演进——而它们任何
     一处漏转义，症状都是页面注入，不是「两份不一致」这种看得见的问题。 */
  var esc = AF.esc;

  /* =========================================================================
     §3 外壳装配
     -------------------------------------------------------------------------
     同步执行：页面 markup 是 <div data-shell>…</div>，<script> 在它之后。
     没有 DOMContentLoaded，所以没有执行顺序问题（D7）。
     ======================================================================= */
  AF.mount = function (page, opts) {
    var host = document.querySelector('[data-shell]');
    if (!host) return null;

    var app = document.createElement('div');
    app.className = 'af-app';
    app.setAttribute('data-rail', 'open');
    app.setAttribute('data-page', page);
    app.innerHTML = shellHTML(page, opts);

    /* 页面自带的内容搬进 <main> */
    var main = app.querySelector('[data-role="main"]');
    while (host.firstChild) main.appendChild(host.firstChild);

    host.appendChild(app);
    document.body.classList.add('af-shelled');

    bindShell(app);
    return app;
  };

  function bindShell(app) {
    /* 侧栏折叠：Ctrl+B 与按钮都能开合 */
    function toggleRail() {
      var rail = app.getAttribute('data-rail');
      app.setAttribute('data-rail', rail === 'open' ? 'collapsed' : 'open');
      var t = app.querySelector('[data-role="rail-toggle"]');
      if (t) t.focus();
    }
    app.addEventListener('click', function (e) {
      var t = e.target.closest('[data-role="rail-toggle"], [data-role="rail-collapse"]');
      if (t) { e.preventDefault(); toggleRail(); }
      var h = e.target.closest('[data-role="help-open"]');
      if (h) { e.preventDefault(); openHelp(h); }
    });
    app.__toggleRail = toggleRail;
    shellApp = app;

    /* 焦点落到主区后，把 h1 交给辅助技术（规范 §4.2） */
    mainFocus(app);

    /* / 聚焦搜索：handler 在文件级注册（见上方），这里不再注册 */
  }

  /* 侧栏折叠 / 帮助 / 搜索的 handler 在文件级注册一次（不是每次 mount），
     否则从带外壳的页面跳到不带外壳的页面（index.html）时，
     `?` 面板会把它们标成死键——但 handler 又确实不存在。
     现在它们永远有 handler，只是行为取决于当前页有没有外壳。 */
  var shellApp = null;
  AF.register('search:focus', function () {
    var s = shellApp && shellApp.querySelector('[data-role="search"]');
    if (s) { s.focus(); s.select(); return true; }
    return false;
  });
  AF.register('rail:toggle', function () {
    if (shellApp && shellApp.__toggleRail) { shellApp.__toggleRail(); return true; }
    return false; /* 无外壳页：Ctrl+B 不该做事，但也无害 */
  });

  function mainFocus(app) {
    var main = app.querySelector('[data-role="main"]');
    if (!main) return;
    main.addEventListener('focus', function () {
      var h1 = main.querySelector('h1');
      if (h1) h1.setAttribute('tabindex', '-1');
    });
  }

  /* =========================================================================
     §4 首焦点：跳转后的落点（D1）
     -------------------------------------------------------------------------
     上一版 12 个页面 0 个 autofocus、0 个 main[tabindex=-1]：
     跳转后焦点回到 <body>，屏幕阅读器不播报新页面，键盘用户 Tab 一轮
     才回到侧栏。这里统一收口：main 聚焦（它把焦点从 <body> 拿出来），
     并把 h1 的 tabindex 补成 -1，让「Shift+Tab 回到标题」成为可能。
     ======================================================================= */
  AF.firstFocus = function () {
    var main = document.querySelector('main[tabindex="-1"]');
    if (main) {
      var h1 = main.querySelector('h1');
      if (h1) h1.setAttribute('tabindex', '-1');
      main.focus();
    }
  };

  document.addEventListener('DOMContentLoaded', function () {
    AF.firstFocus();
    var dead = AF.unimplemented();
    if (dead.length && window.console) {
      console.warn('[keynav] 表里有键位但没有 handler：', dead.join(', '));
    }
  });

  /* =========================================================================
     §5 Mock 数据（全站唯一一份）
     -------------------------------------------------------------------------
     字段对齐 crates/hub-storage/src/schema.sql 的 device 列。
     status 三态：online / reconnecting / offline。reconnecting 只在
     Hub 内存里，前端按「WS 断开 + 心跳年龄 ≤45s」自推——所以这里
     多一个 derived 字段告诉读者它是推出来的，不是存下来的。
     ======================================================================= */
  var MOCK = {
    hub: { version: '0.1.0', channel: 'dev', uptime: '6d 14h', bind: '127.0.0.1:8080' },

    devices: [
      { id: 'dev-mbp', name: 'mbp.local', platform: 'darwin/arm64', agent_version: '0.1.0',
        status: 'online', last_seen: '12s', cpu: 34, mem: 61, gpu: false,
        agents: ['claude-code', 'codex', 'aider'] },
      { id: 'dev-linux', name: 'build-box', platform: 'linux/amd64', agent_version: '0.1.0',
        status: 'online', last_seen: '4s', cpu: 78, mem: 83, gpu: true,
        agents: ['claude-code', 'codex'] },
      { id: 'dev-rpi', name: 'rpi-edge', platform: 'linux/arm64', agent_version: '0.1.0',
        status: 'reconnecting', derived: true, last_seen: '31s', cpu: 0, mem: 0, gpu: false,
        agents: ['claude-code'] },
      { id: 'dev-win', name: 'win-desktop', platform: 'windows/amd64', agent_version: '0.0.9',
        status: 'offline', last_seen: '3h 21m', cpu: 0, mem: 0, gpu: false,
        agents: [] }
    ],

    agents: [
      { id: 'claude-code', label: 'Claude Code', vendor: 'Anthropic',
        desc: '终端里的编程 agent。读写文件、跑命令、多轮修改。',
        requires: ['git', 'node>=20'], notes: '默认 agent；装好即可用。' },
      { id: 'codex', label: 'Codex CLI', vendor: 'OpenAI',
        desc: '轻量、快，适合改小文件与跑脚本。',
        requires: ['node>=18'], notes: '需要 OPENAI_API_KEY。' },
      { id: 'aider', label: 'Aider', vendor: 'community',
        desc: 'git 仓库内的结对修改，diff 先行确认。',
        requires: ['git', 'python>=3.10'], notes: '只在 git 仓库内工作。' }
    ],

    sessions: [
      { id: 's-7f3a', title: '修 ws 断线重连', device: 'dev-mbp', agent: 'claude-code',
        path: '~/src/AgentFleet', status: 'active', started: '14 分钟前', events: 128,
        panels: ['terminal'] },
      { id: 's-1c02', title: '重构 hub-storage 迁移', device: 'dev-linux', agent: 'codex',
        path: '/srv/AgentFleet', status: 'active', started: '2 小时前', events: 431,
        panels: ['terminal', 'files'] },
      { id: 's-9d55', title: '给 identity 补测试', device: 'dev-mbp', agent: 'aider',
        path: '~/src/AgentFleet', status: 'disconnected', started: '昨天', events: 87,
        panels: [] },
      { id: 's-3e81', title: '文档：M1 设计规范', device: 'dev-mbp', agent: 'claude-code',
        path: '~/src/AgentFleet', status: 'ended', started: '3 天前', events: 312,
        panels: [] },
      { id: 's-2b74', title: '迁移 SQLite → WAL 模式', device: 'dev-linux', agent: 'claude-code',
        path: '/srv/AgentFleet', status: 'ended', started: '4 天前', events: 156,
        panels: [] },
      { id: 's-6a19', title: '设备心跳超时调参', device: 'dev-rpi', agent: 'claude-code',
        path: '/opt/edge-agent', status: 'ended', started: '上周', events: 64,
        panels: [] },
      { id: 's-8c33', title: '清理 coverage 例外登记', device: 'dev-mbp', agent: 'codex',
        path: '~/src/AgentFleet', status: 'ended', started: '上周', events: 203,
        panels: [] },
      { id: 's-4f07', title: 'rpi-edge 温度监控脚本', device: 'dev-rpi', agent: 'aider',
        path: '/opt/edge-agent', status: 'ended', started: '两周前', events: 91,
        panels: [] }
    ],

    /* 轨迹事件 = append-only 流水；对话是它按 kind 折叠后的读法。
       两者逐条对齐，所以这里是同一份数组。seq 越大越新，
       渲染时按 seq 倒序（最新在上）或正序（时间线）由页面决定。 */
    trajectory: [
      { seq: 128, kind: 'tool', who: 'agent', text: 'Read crates/hub-server/src/routes/ws.rs', ms: 42 },
      { seq: 127, kind: 'say', who: 'agent', text: '重连逻辑在 relay() 里，我先看清它怎么处理 incoming。', ms: 0 },
      { seq: 126, kind: 'tool', who: 'agent', text: 'Grep "select!" crates/hub-server/src/routes/ws.rs', ms: 8 },
      { seq: 125, kind: 'say', who: 'user', text: 'ws 断了重连不上，帮我看看 relay() 里的 incoming 处理。', ms: 0 },
      { seq: 124, kind: 'note', who: 'system', text: 'PTY 尺寸协商 120x36', ms: 0 },
      { seq: 123, kind: 'tool', who: 'agent', text: 'Bash cargo test --workspace', ms: 18420 },
      { seq: 122, kind: 'say', who: 'agent', text: '测试全绿。接下来查 flood_frame 的映射。', ms: 0 },
      { seq: 121, kind: 'tool', who: 'agent', text: 'Edit crates/hub-server/src/routes/ws.rs +6 −2', ms: 12 },
      { seq: 120, kind: 'say', who: 'agent', text: '问题在 select! 的 incoming 臂：断开时的空文本被当成消息处理了。', ms: 0 },
      { seq: 119, kind: 'note', who: 'system', text: '会话已建立 · mbp.local · ~/src/AgentFleet', ms: 0 }
    ]
  };

  AF.mock = MOCK;

  /* =========================================================================
     §6 设备三态（一个决定只有一处）
     -------------------------------------------------------------------------
     四个通道：颜色 + 字形 + 填充 + 文字。色觉障碍、灰度打印、8px 缩略、
     reduce-motion 下都仍能区分——所以任何一个通道都不单独承担识别。
     ======================================================================= */
  var STATE = {
    online:        { glyph: '●', cls: 'ok',      label: '在线' },
    reconnecting:  { glyph: '◐', cls: 'warn',   label: '重连中' },
    offline:       { glyph: '○', cls: 'danger',  label: '离线' },
    active:        { glyph: '●', cls: 'ok',      label: '运行中' },
    disconnected:  { glyph: '◐', cls: 'warn',   label: '断线' },
    ended:         { glyph: '○', cls: 'off',     label: '已结束' }
  };

  AF.stateHTML = function (state, extra) {
    var s = STATE[state] || { glyph: '◌', cls: 'off', label: state };
    var derived = extra && extra.derived;
    return '<span class="af-state" data-state="' + state + '">' +
      '<span class="af-state__glyph" data-glyph="' + (s.glyph === '●' ? 'filled' : s.glyph === '○' ? 'hollow' : 'partial') +
        '" aria-hidden="true">' + s.glyph + '</span>' +
      '<span>' + s.label + '</span>' +
      (derived ? '<span class="af-badge af-badge--off" title="Hub 内存推导，非持久化">推</span>' : '') +
      '</span>';
  };

  /* =========================================================================
     §7 Toast
     -------------------------------------------------------------------------
     可恢复的失败（离线、越界、保存冲突）走 sticky：不自动消失，带动作。
     自动消失只用于「已经成功的通知」——那种东西不需要用户做什么。
     ======================================================================= */
  var toastHost = null;
  AF.toast = function (o) {
    if (!toastHost) {
      toastHost = document.createElement('div');
      toastHost.className = 'af-toasts';
      toastHost.setAttribute('role', 'log');
      toastHost.setAttribute('aria-live', 'polite');
      document.body.appendChild(toastHost);
    }
    var el = document.createElement('div');
    el.className = 'af-toast';
    el.setAttribute('data-tone', o.tone || 'info');
    el.setAttribute('role', o.sticky ? 'alert' : 'status');
    el.innerHTML =
      '<div class="af-toast__body">' +
        '<span class="af-toast__title">' + esc(o.title) + '</span>' +
        (o.text ? '<span class="af-toast__text">' + esc(o.text) + '</span>' : '') +
        (o.actions ? '<span class="af-toast__actions"></span>' : '') +
      '</div>' +
      (o.sticky ? '<button class="af-btn af-btn--ghost af-btn--sm" type="button" data-toast-close aria-label="关闭">✕</button>' : '');
    if (o.actions) {
      var wrap = el.querySelector('.af-toast__actions');
      o.actions.forEach(function (a) {
        var b = document.createElement('button');
        b.className = 'af-btn af-btn--sm' + (a.primary ? ' af-btn--primary' : '');
        b.type = 'button';
        b.textContent = a.label;
        b.addEventListener('click', function () { a.fn(); close(); });
        wrap.appendChild(b);
      });
    }
    function close() { el.remove(); }
    el.addEventListener('click', function (e) {
      if (e.target.closest('[data-toast-close]')) close();
    });
    toastHost.appendChild(el);
    if (!o.sticky) setTimeout(close, o.ms || 3200);
    return close;
  };

  /* =========================================================================
     §8 键位帮助面板
     -------------------------------------------------------------------------
     内容由 keynav.js 的 KEYS 渲染（AF.renderHelp）。这里只负责开与关，
     且**必须走焦点栈**——上一版 trapFocus 的 bug 就是绕过栈的后果。
     ======================================================================= */
  var helpEl = null;
  function openHelp(opener) {
    if (helpEl) { AF.focusPop(helpEl); return; }
    helpEl = document.createElement('div');
    helpEl.className = 'af-help';
    helpEl.setAttribute('role', 'dialog');
    helpEl.setAttribute('aria-modal', 'true');
    helpEl.setAttribute('aria-label', '键位帮助');
    helpEl.innerHTML =
      '<div class="af-help__box">' +
        '<div class="af-help__head">' +
          '<span class="af-t-label">键位</span>' +
          '<span class="af-spacer"></span>' +
          '<span class="af-t-mute" style="font-size:var(--af-text-xs)">' +
            '输入框内单键不抢；修饰键照常生效。这张表就是实现。</span>' +
          '<button class="af-btn af-btn--ghost af-btn--sm af-btn--icon" type="button" ' +
            'data-help-close aria-label="关闭">✕</button>' +
        '</div>' +
        '<div class="af-help__body">' + AF.renderHelp() + '</div>' +
      '</div>';
    helpEl.addEventListener('click', function (e) {
      if (e.target === helpEl || e.target.closest('[data-help-close]')) AF.focusPop(helpEl);
    });
    document.body.appendChild(helpEl);
    AF.focusPush(helpEl, { opener: opener });
    helpEl.addEventListener('transitionend', function () {}, { once: true });
    /* 关闭时清理引用，否则下次打开是空壳 */
    var mo = new MutationObserver(function () {
      if (!document.contains(helpEl)) { helpEl = null; mo.disconnect(); }
    });
    mo.observe(document.body, { childList: true });
  }

  AF.register('help:toggle', function (e) {
    if (helpEl) AF.focusPop(helpEl);
    else openHelp(document.activeElement);
    return true;
  });

  /* =========================================================================
     §9 列表导航（焦点真的落在行上）
     -------------------------------------------------------------------------
     行是 <button data-row tabindex="-1">，容器是 tab stop（tabindex="0"）。
     ↑↓ 移动焦点 + 写 aria-selected（辅助技术可读选中项）。
     Enter 的语义由行自己决定：data-href 跳转、内含 button/a 就点它、
     否则抛一个 af:enter 事件给页面接。
     ======================================================================= */
  AF.register('list:move', function (e, delta) {
    return AF.moveSelection(delta || 1);
  });

  AF.register('list:enter', function (e) {
    var row = AF.currentRow() || (document.activeElement && document.activeElement.closest('[data-row]'));
    if (!row) return false;
    if (row.dataset.href) { location.href = row.dataset.href; return true; }
    var inner = row.querySelector('a[href], button');
    if (inner) { inner.click(); return true; }
    row.dispatchEvent(new CustomEvent('af:enter', { bubbles: true, detail: { row: row } }));
    return true;
  });

  AF.register('list:toggle', function () {
    var row = AF.currentRow() || (document.activeElement && document.activeElement.closest('[data-row]'));
    if (!row) return false;
    if (row.hasAttribute('aria-expanded')) {
      var open = row.getAttribute('aria-expanded') === 'true';
      row.setAttribute('aria-expanded', open ? 'false' : 'true');
      var kids = row.parentElement.querySelector('[data-kids]');
      if (kids) kids.hidden = open;
      return true;
    }
    var sel = row.getAttribute('aria-selected') === 'true';
    row.setAttribute('aria-selected', sel ? 'false' : 'true');
    row.dispatchEvent(new CustomEvent('af:toggle', { bubbles: true, detail: { row: row, on: !sel } }));
    return true;
  });

  /* =========================================================================
     §10 面板坞 / 轨迹抽屉（会话工作台专用）
     -------------------------------------------------------------------------
     这三件在别的页面也注册了 handler 吗？没有——register 检查 act 在表里，
     且同一 act 不许注册两次，所以「在设备页按 o 没反应」是**正确行为**，
     而不是上一版那种「表里有、实现里没有」的死键。
     ======================================================================= */
  AF.register('panel:open', function () { return AF.workbench && AF.workbench.openPanel(); });
  AF.register('panel:prev', function () { return AF.workbench && AF.workbench.cyclePanel(-1); });
  AF.register('panel:next', function () { return AF.workbench && AF.workbench.cyclePanel(1); });
  AF.register('dock:toggle', function () { return AF.workbench && AF.workbench.toggleDock(); });
  AF.register('traj:toggle', function () { return AF.workbench && AF.workbench.toggleTraj(); });
  AF.register('term:clear', function () { return AF.workbench && AF.workbench.clearTerm(); });
  AF.register('file:up', function () { return AF.workbench && AF.workbench.fileUp(); });
  AF.register('file:save', function () { return AF.workbench && AF.workbench.fileSave(); });

  /* =========================================================================
     §11 g 序列跳转（唯一的导航快捷键）
     -------------------------------------------------------------------------
     跳转 = location.href，所以它是「真的跳走了」，不是切 tab。
     这也是键位表里唯一会离开当前页的一组——所以在 `?` 里它
     单独归在「跳转」组，不与同页动作混在一起。
     ======================================================================= */
  [['nav:sessions', 'sessions.html'], ['nav:new-session', 'new-session.html'],
   ['nav:devices', 'devices.html'], ['nav:agents', 'agents.html'],
   ['nav:settings', 'settings.html']].forEach(function (p) {
    AF.register(p[0], function () { location.href = p[1]; return true; });
  });

  /* =========================================================================
     §12 Modal（走焦点栈）
     -------------------------------------------------------------------------
     与上一版的区别：不缓存 focusables、每次 Tab 重算（keynav.js §4），
     且背后内容被 inert——所以 Tab 出不去，Esc 一定回来。
     ======================================================================= */
  AF.modal = function (o) {
    var scrim = document.createElement('div');
    scrim.className = 'af-scrim';
    scrim.innerHTML =
      '<div class="af-modal" role="dialog" aria-modal="true" aria-label="' + esc(o.title) + '">' +
        '<div class="af-modal__head">' +
          '<span class="af-panel__title">' + esc(o.title) + '</span>' +
          '<span class="af-spacer"></span>' +
          (o.badge || '') +
          '<button class="af-btn af-btn--ghost af-btn--sm af-btn--icon" type="button" ' +
            'data-modal-close aria-label="关闭">✕</button>' +
        '</div>' +
        '<div class="af-modal__body">' + (o.body || '') + '</div>' +
        (o.actions ? '<div class="af-modal__foot"><span class="af-spacer"></span></div>' : '') +
      '</div>';
    scrim.addEventListener('click', function (e) {
      if (e.target === scrim || e.target.closest('[data-modal-close]')) close();
    });
    if (o.actions) {
      var foot = scrim.querySelector('.af-modal__foot');
      o.actions.forEach(function (a) {
        var b = document.createElement('button');
        b.className = 'af-btn' + (a.primary ? ' af-btn--primary' : '') + (a.danger ? ' af-btn--danger' : '');
        b.type = 'button';
        b.textContent = a.label;
        b.addEventListener('click', function () { a.fn(close); });
        foot.appendChild(b);
      });
    }
    document.body.appendChild(scrim);
    AF.focusPush(scrim);
    return close;
    function close() { AF.focusPop(scrim); }
  };

  /* =========================================================================
     §13 越界反馈（四路可见证据之一）
     -------------------------------------------------------------------------
     点击被排除的节点 → 解释性错误面板，而不是「什么都不发生」。
     上一版这里是静默的：用户点一下没反应，只会以为自己点歪了。
     ======================================================================= */
  AF.explainOutOfScope = function (opener, path, reason) {
    AF.modal({
      title: '这个路径在工作区之外',
      badge: '<span class="af-badge af-badge--danger">越界</span>',
      body:
        '<div class="af-col gap-4">' +
          '<div><span class="af-t-label">路径</span><div class="af-fp" style="margin-top:4px">' + esc(path) + '</div></div>' +
          '<div><span class="af-t-label">原因</span><div style="margin-top:4px;color:var(--af-fg-primary)">' + esc(reason) + '</div></div>' +
          '<div class="af-field__hint">越界不是「暂时不能访问」，是这台 agent 的会话边界外的东西。' +
            '要访问它，请在<a href="new-session.html">新建会话</a>时把它选进工作区路径。</div>' +
        '</div>',
      actions: [
        { label: '知道了', primary: true },
        { label: '去新建会话', fn: function (close) { close(); location.href = 'new-session.html'; } }
      ]
    });
  };

  AF.explain = AF.explainOutOfScope;

  /* =========================================================================
     §14 折叠侧栏的惰性焦点
     -------------------------------------------------------------------------
     折叠后导航项只剩图标，按钮的 aria-label 必须补全——否则屏幕阅读器
     读到的是空按钮。这件事在 CSS 里做不了（内容被 display:none），
     在注入时给每个按钮挂 aria-label。
     ======================================================================= */
  document.addEventListener('DOMContentLoaded', function () {
    [].forEach.call(document.querySelectorAll('.af-sidebar__item'), function (a) {
      var text = a.querySelector('.af-sidebar__item-text');
      if (text && !a.getAttribute('aria-label')) a.setAttribute('aria-label', text.textContent);
    });
  });

})();
