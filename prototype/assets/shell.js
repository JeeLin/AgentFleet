/* ===========================================================================
   shell.js —— 外壳注入（左侧导航 + 原型导览条）与管家开关
   ---------------------------------------------------------------------------
   每页只写 <body data-nav="board">，导航由这里渲染。导航只有一份：
   抄多份就会改一次改多处，而原型正是拿来评审信息架构的。
   mock 数据集中于此，不假装在连后端。
   =========================================================================== */

/* 项目在这里是「主轴」，所以侧栏第一组就是项目本身 */
const PROJECTS = [
  { id: 'afleet',  name: 'AgentFleet',  shape: '中心式', color: 'dot-a' },
  { id: 'notes',   name: '个人笔记站',  shape: '镜像式', color: 'dot-b' },
  { id: 'metrics', name: '内部指标台',  shape: '隔离式', color: 'dot-c' },
];

const NAV = [
  { group: '全站', items: [
      { href: 'index.html', label: '总览', key: 'home' },
      /* 归档项目必须找得回来：设置页和建项目页都写了「随时能取消归档」，
         之前却没有一处能看到它们 —— 承诺不能是空头的。 */
      { href: 'archived.html', label: '已归档', key: 'archived' },
  ] },
  { group: '项目', items: PROJECTS.map((p) => ({
      href: `board.html?project=${p.id}`, label: p.name, dot: p.color, key: `board:${p.id}`,
  })) },
  { group: '收件箱', items: [
      { href: 'inbox.html', label: '待决策', badge: '3', key: 'inbox' },
      { href: 'inbox-activity.html', label: '动态', key: 'inbox-activity' },
  ] },
  { group: '管理面', items: [
      { href: 'devices.html', label: '设备', key: 'devices' },
      { href: 'agents.html', label: 'Agent', key: 'agents' },
      /* 凭据和授予得有地方管：Agent 页只讲「为什么不让 agent 直接连」的原则，
         真正要加一个数据库、看谁用过它，得有一页。 */
      { href: 'skills.html', label: '技能与 MCP', key: 'skills' },
      { href: 'schedules.html', label: '定时任务', key: 'schedules' },
      { href: 'integrations.html', label: '外部来源', key: 'integrations' },
      { href: 'settings.html', label: '设置', key: 'settings' },
  ] },
  /* 通知相关的东西（出口配置、IM 里长什么样）不是「管理某台机器」，
     也不是「某个项目」，是全局的沟通方式。单列一组，免得埋进设置里找不到。 */
  { group: '通知', items: [
      { href: 'notifications.html', label: '通知出口', key: 'notifications' },
      { href: 'butler-im.html', label: 'IM 里的管家', key: 'butler-im' },
  ] },
];

/* 导览条：原型要评审的是「整站」，所以每页都可达 */
const PAGES = [
  { href: 'index.html',           label: '总览' },
  { href: 'archived.html',        label: '已归档' },
  { href: 'project-new.html',     label: '建项目' },
  { href: 'project-bind.html',    label: '绑定设备' },
  { href: 'board.html',           label: '看板' },
  { href: 'project-settings.html',label: '项目设置' },
  { href: 'timeline.html',        label: '时间线' },
  { href: 'task.html',            label: '任务' },
  { href: 'execution.html',       label: '执行监控' },
  { href: 'replay.html',          label: '执行复盘' },
  { href: 'inbox.html',           label: '收件箱' },
  { href: 'inbox-activity.html',  label: '动态' },
  { href: 'devices.html',         label: '设备' },
  { href: 'agents.html',          label: 'Agent' },
  { href: 'skills.html',          label: '技能与 MCP' },
  { href: 'settings.html',        label: '设置' },
  { href: 'notifications.html',   label: '通知出口' },
  { href: 'butler-im.html',       label: 'IM 管家' },
  { href: 'integrations.html',    label: '外部来源' },
  { href: 'schedules.html',       label: '定时任务' },
  { href: 'session.html',         label: '会话' },
];

const ROOT = location.pathname.includes('/panels/') ? '../' : '';

/* 换页时把当前配色带上，否则一切页就跳回默认那版，比较就没法做了 */
function withTheme(href) {
  const t = new URLSearchParams(location.search).get('theme');
  if (!t) return href;
  const sep = href.includes('?') ? '&' : '?';
  return `${href}${sep}theme=${t}`;
}

const href = (p) => withTheme(ROOT + p);

function el(html) {
  const t = document.createElement('template');
  t.innerHTML = html.trim();
  return t.content.firstElementChild;
}

/* 导航项列表：左栏和底部面板共用这一份渲染，否则两边迟早会漂。
   参数 tag 只是让面板里多套一层容器，样式靠 .af-rail__group/__list 复用。 */
function renderNavItems(active) {
  const frag = document.createDocumentFragment();
  for (const g of NAV) {
    /* 「项目」这一组的标题右侧挂一个 +：建项目是高频动作，
       不该逼人先回总览页再找按钮。 */
    const groupEl = el(`
      <div class="af-rail__group">
        <span>${g.group}</span>
        ${g.group === '项目'
          ? '<a class="af-rail__add" href="' + href('project-new.html') + '" title="新建项目" aria-label="新建项目">+</a>'
          : ''}
      </div>
    `);
    frag.appendChild(groupEl);
    const ul = el('<ul class="af-rail__list"></ul>');
    for (const it of g.items) {
      /* 看板页按 ?project= 高亮对应项目，否则「项目」这一组看不出你在哪个项目里 */
      const key = it.key;
      const isCurrent = key === active ||
        (key.startsWith('board:') && active.startsWith('board:') && key === active);
      ul.appendChild(el(`
        <li>
          <a href="${href(it.href)}" ${isCurrent ? 'aria-current="page"' : ''}>
            ${it.dot ? `<span class="af-dot ${it.dot}" aria-hidden="true"></span>` : ''}
            <span class="af-rail__label">${it.label}</span>
            ${it.badge ? `<span class="af-tag af-tag--failed">${it.badge}</span>` : ''}
          </a>
        </li>
      `));
    }
    frag.appendChild(ul);
  }
  return frag;
}

const navBtnHTML = (extraClass, attrs, label) => `
  <button class="af-navbtn ${extraClass}" type="button" ${attrs} aria-label="${label}" title="${label}">
    <span class="af-navbtn__bars" aria-hidden="true"><span></span><span></span><span></span></span>
  </button>`;

function renderRail(active) {
  const rail = el('<aside class="af-rail" aria-label="主导航"></aside>');
  rail.appendChild(el(`
    <div class="af-rail__brand">
      <span class="af-rail__mark" aria-hidden="true">AF</span>
      <span>
        <span class="af-rail__name">AgentFleet</span><br>
        <span class="af-rail__sub">self-hosted</span>
      </span>
      ${navBtnHTML('', 'data-rail-toggle', '收起导航')}
    </div>
  `));
  rail.appendChild(renderNavItems(active));
  rail.appendChild(el(`
    <div class="af-rail__foot">
      单用户 · Hub 本地<br>
      3 台设备 · 6 个 Agent · <span class="af-mono">v0.6.0</span>
    </div>
  `));
  return rail;
}

/* 左栏收起后浮在原地的那颗按钮：没有它，收起来就再也点不回去。
   ---------------------------------------------------------------------------
   它同时兼任「页面清单」的开关 —— 这两个动作本来都是「把导航请出来」，
   分成两颗按钮摆在同一个角落，人会分不清哪颗管什么。

   所以合成一颗，内部两个点击区：
     左半（三条横线）→ 展开左栏
     右半（向下箭头）→ 展开 16 页清单
   中间一道分隔线说明这是两块，不是一块。

   不用单击/双击区分：双击在手机上要等 300ms 才确认（浏览器得先判断
   你是不是在双击缩放），那 300ms 会落在所有单击上，感觉像卡住；
   而且双击没有任何视觉暗示，没人会对着按钮连点两下试试。

   左栏开着时整颗不显示（display:none）—— 那时左边的三横线没有意义。 */
function renderRecallBtn() {
  return el(`
    <div class="af-recallsplit">
      <button class="af-recallsplit__rail" type="button" data-rail-toggle
              aria-label="展开导航" title="展开导航（Ctrl+B）">
        <span class="af-navbtn__bars" aria-hidden="true"><span></span><span></span><span></span></span>
      </button>
      <span class="af-recallsplit__sep" aria-hidden="true"></span>
      <button class="af-recallsplit__pages" type="button" aria-expanded="false"
              aria-label="全部页面" title="全部页面">
        <span class="af-recallsplit__arrow" aria-hidden="true"></span>
      </button>
    </div>
  `);
}

/* 窄屏常驻按钮：点开底部导航面板。它是个链接（href="#nav"），
   这样开关完全不依赖 JS —— 面板的显隐是纯 CSS :target。 */
function renderFloatBtn() {
  return el(`
    <a class="af-navbtn af-navbtn--float" href="#nav" aria-label="打开导航" title="打开导航">
      <span class="af-navbtn__bars" aria-hidden="true"><span></span><span></span><span></span></span>
    </a>
  `);
}

/* 底部导航面板：窄屏的完整导航。桌面下不显示（左栏已经在），
   所以不用 aria-hidden 做取舍 —— CSS 的 display:none 天然把两边分开，
   不会出现两份 aria-current="page" 同时被读到。 */
function renderNavPanel(active) {
  const panel = el(`
    <div class="af-navpanel" id="nav" role="dialog" aria-modal="true" aria-label="导航">
      <div class="af-navpanel__grab" aria-hidden="true"></div>
      <div class="af-navpanel__head">
        <span class="af-navpanel__title">导航</span>
        <a class="af-btn af-btn--ghost af-btn--sm af-navpanel__close" href="#nav-closed">关闭</a>
      </div>
      <div class="af-navpanel__body"></div>
      <div class="af-navpanel__foot">
        单用户 · Hub 本地<br>
        3 台设备 · 6 个 Agent · <span class="af-mono">v0.6.0</span>
      </div>
    </div>
  `);
  panel.querySelector('.af-navpanel__body').appendChild(renderNavItems(active));
  /* 蒙层本身是指向 #nav-closed 的链接：点它就关。
     这样「点外面关掉」也不用 JS。 */
  return panel;
}

function renderNavScrim() {
  return el('<a class="af-navscrim" href="#nav-closed" aria-label="关闭导航"></a>');
}

/* 全部页面清单：16 页平铺。
   ---------------------------------------------------------------------------
   原来自称「原型」，是个虚线小标签浮在左下角。现在它不再是独立按钮，
   而是「唤回按钮」右半那块展开的内容 —— 一个角落只有一个入口。

   面板从按钮上方升起（bottom: 100%），不是从屏幕底部 ——
   因为按钮本身就在左下角，从它头顶弹出来才看得出「这是它开的」。 */
function renderPageList() {
  const box = el('<div class="af-pagelist" id="pagelist" role="dialog" aria-label="全部页面"></div>');
  const head = el(`
    <div class="af-pagelist__head">
      <span class="af-pagelist__title">全部页面</span>
      <span class="af-pagelist__note">评审用索引</span>
    </div>
  `);
  box.appendChild(head);
  const list = el('<div class="af-pagelist__body"></div>');
  const here = location.pathname.split('/').pop() || 'index.html';
  for (const p of PAGES) {
    const cur = here === p.href.split('/').pop() ? 'aria-current="page"' : '';
    list.appendChild(el(`<a href="${href(p.href)}" ${cur}>${p.label}</a>`));
  }
  box.appendChild(list);
  return box;
}

/* ---------------------------------------------------------------------------
   左栏收起 / 展开
   收起是长期偏好（不是「这次先关一下」），所以记在 localStorage。
   存在 <html data-rail="off">，CSS 据此把整个左栏滑出去。
   窄屏没有「收起」这回事（左栏根本不渲染），按钮改走底部面板，所以窄屏忽略这个状态。
   --------------------------------------------------------------------------- */
const RAIL_KEY = 'af-rail';

function applyRail(state) {
  if (state === 'off') document.documentElement.dataset.rail = 'off';
  else delete document.documentElement.dataset.rail;
  document.querySelectorAll('[data-rail-toggle]').forEach((b) => {
    const off = state === 'off';
    b.setAttribute('aria-label', off ? '展开导航' : '收起导航');
    b.setAttribute('title', off ? '展开导航' : '收起导航');
    b.setAttribute('aria-expanded', String(!off));
  });
}

document.addEventListener('DOMContentLoaded', () => {
  /* 侧栏高亮：body[data-nav] 给了 key；看板页再按 ?project= 细分 */
  let nav = document.body.dataset.nav || '';
  if (nav === 'board') {
    const q = new URLSearchParams(location.search).get('project') || 'afleet';
    nav = `board:${q}`;
  }
  const shell = document.querySelector('.af-shell');
  if (shell) shell.insertBefore(renderRail(nav), shell.firstChild);

  let saved = 'on';
  try { saved = localStorage.getItem(RAIL_KEY) || 'on'; } catch (e) { /* 隐私模式下会抛，忽略 */ }
  applyRail(saved);

  shell.appendChild(renderFloatBtn());          // 窄屏常驻
  shell.appendChild(renderRecallBtn());         // 桌面收起后浮出（内含页面清单开关）
  shell.appendChild(renderPageList());          // 全部页面清单
  shell.appendChild(renderNavPanel(nav));       // 窄屏底部面板
  shell.appendChild(renderNavScrim());          // 面板遮罩

  document.querySelectorAll('[data-rail-toggle]').forEach((b) => {
    b.addEventListener('click', () => {
      const off = document.documentElement.dataset.rail === 'off';
      const next = off ? 'on' : 'off';
      try { localStorage.setItem(RAIL_KEY, next); } catch (e) { /* 同上 */ }
      applyRail(next);
      /* 左栏一展开，这颗按钮就整颗不显示了，页面清单必须跟着收起 ——
         否则会出现「清单浮着、按钮没了、关不掉」 */
      if (next === 'on') closePageList();
    });
  });

  /* 页面清单开关：右半那块小箭头 */
  const listBtn = document.querySelector('.af-recallsplit__pages');
  const listBox = document.querySelector('.af-pagelist');
  if (listBtn && listBox) {
    listBtn.addEventListener('click', () => {
      const open = listBox.dataset.open === 'true';
      if (open) closePageList();
      else openPageList();
    });
  }
  /* 点清单外面关掉它。用 pointerdown 而不是 click：
     清单里的链接如果在 click 阶段已经跳走了，这里就不用管了。 */
  addEventListener('pointerdown', (e) => {
    if (!listBox || listBox.dataset.open !== 'true') return;
    if (listBox.contains(e.target) || e.target.closest('.af-recallsplit__pages')) return;
    closePageList();
  });
  addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && listBox && listBox.dataset.open === 'true') closePageList();
  });
});

function openPageList() {
  const box = document.querySelector('.af-pagelist');
  const btn = document.querySelector('.af-recallsplit__pages');
  if (!box) return;
  box.dataset.open = 'true';
  if (btn) { btn.setAttribute('aria-expanded', 'true'); btn.setAttribute('title', '收起全部页面'); }
}

function closePageList() {
  const box = document.querySelector('.af-pagelist');
  const btn = document.querySelector('.af-recallsplit__pages');
  if (!box) return;
  box.dataset.open = 'false';
  if (btn) { btn.setAttribute('aria-expanded', 'false'); btn.setAttribute('title', '全部页面'); }
}

/* Ctrl/Cmd+B 开合左栏：编辑器里的肌肉记忆，顺手给一个不用找按钮的入口。
   窄屏下不做任何事（没有可收起的左栏，弹底部面板留给按钮）。

   这里判断「是不是窄屏」不用 matchMedia —— 它比 CSS 的媒体查询多一个
   失效面（老 WebView、非浏览器环境根本没有它），一旦缺失整个快捷键就废了。
   改成问 CSS：左栏如果被媒体查询设成 display:none，说明现在是窄屏。
   这样判断依据只有一个来源，就是那份 @media，不会两处漂。 */
addEventListener('keydown', (e) => {
  try {
    if (!(e.ctrlKey || e.metaKey) || String(e.key).toLowerCase() !== 'b') return;
    const rail = document.querySelector('.af-rail');
    if (!rail) return;
    if (getComputedStyle(rail).display === 'none') return;   // 窄屏：左栏本来就不显示
    e.preventDefault();
    const next = document.documentElement.dataset.rail === 'off' ? 'on' : 'off';
    try { localStorage.setItem(RAIL_KEY, next); } catch (err) { /* 隐私模式 */ }
    applyRail(next);
  } catch (err) {
    /* 快捷键是锦上添花，坏了也不能连累点击那条路 */
  }
});

window.AF_PROTO = { PROJECTS, NAV, PAGES };

/* ---------------------------------------------------------------------------
   会话页：让标签的选中态跟着地址里的 #pane-* 走。
   面板的显示/隐藏本身是纯 CSS（:target），这段只负责高亮标签 —— 
   否则会出现「内容变了、标签还亮着旧的」。
   --------------------------------------------------------------------------- */
document.addEventListener('DOMContentLoaded', () => {
  const tabs = document.querySelectorAll('.af-panetab[href^="#pane-"]');
  if (!tabs.length) return;

  function sync() {
    const hash = location.hash;

    /* 导航面板 / 管家抽屉也用 hash（#nav、#butler）。它们不是面板锚点，
       必须跳过 —— 否则打开导航面板会把会话标签的高亮重置成第一个。
       这不是理论问题：会话页在窄屏下点导航按钮，标签就会跳。 */
    if (/^#(nav|butler)/.test(hash)) return;

    const hit = hash && document.querySelector('.af-pane' + hash);

    /* 兜底面板的显示靠这个标记，不靠 CSS 的兄弟选择器猜 ——
       兄弟选择器只能向后匹配，而兜底面板是第一个，后面的标签盖不掉它。 */
    document.body.classList.toggle('af-has-pane', !!hit);

    tabs.forEach((t) => {
      const on = hit && t.getAttribute('href') === hash;
      t.setAttribute('aria-selected', String(!!on));
      t.classList.toggle('is-active', !!on);
    });

    /* 没有合法面板锚点时（首次进来、点了管家、关掉管家），第一个标签算选中 */
    if (!hit) {
      const first = document.querySelector('.af-panetab[href="#pane-terminal"]');
      if (first) { first.setAttribute('aria-selected', 'true'); first.classList.add('is-active'); }
    }
  }

  sync();
  addEventListener('hashchange', sync);
});
