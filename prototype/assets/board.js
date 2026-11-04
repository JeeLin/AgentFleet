/* ===========================================================================
   board.js —— 看板的项目切换与三套数据
   ---------------------------------------------------------------------------
   为什么不在 HTML 里写死：左栏三个项目链接一直是假的 —— 都是
   board.html?project=xxx，但 board.html 从不读这个参数，点哪个项目
   看到的都是 AgentFleet 的看板。这是评审时最容易踩到的坑：
   你会以为自己看了三个项目，其实只看到一个。

   所以改成：数据在这里，按 ?project= 渲染，页面骨架不变。

   三套数据不是随便换个名字。项目形态决定了「Hub 能看到什么」，
   而看板正是「你能看到什么」的地方：
   - 中心式：代码在 Hub 上，diff 完整，验收可信
   - 镜像式：代码在上游，Hub 是镜像，会有「上游已经变了，这里还没拉」的情况
   - 隔离式：代码不落 Hub，但对话 / 改动清单 / diff 都能看（按需向设备拉）。
     它和别的项目真正不一样的是两件事：查看要等设备响应，以及
     那个 commit 的内容 Hub 不替你核对。
   =========================================================================== */

const BOARDS = {
  afleet: {
    name: 'AgentFleet',
    dot: 'dot-a',
    shape: '中心式项目',
    shapeNote: '代码在 Hub，diff 可完整审查',
    bound: '3 台已绑定',
    filters: [
      { label: '设备：device-b', cls: 'af-tag--running' },
      { label: '最近 24 小时有更新', cls: 'af-tag--review' },
    ],
    filterNote: '共 19 张卡 · 3 张被当前筛选隐藏',
    meters: [
      { href: 'task.html', k: '上次做到哪', v: '合入了「看板六列布局」', sub: '2 小时前 · 管家写的总结' },
      { href: 'execution.html', k: '正在跑', v: '<span class="af-num">3</span> 个执行 · <span class="af-num">2</span> 台设备', sub: '最久 18 分 42 秒 · 有 1 个可能卡住' },
      { href: 'inbox.html', k: '等你决定', v: '<span class="af-num">2</span> 个失败待处置 · <span class="af-num">1</span> 张卡待验收', sub: '最早一条已等 40 分钟', action: true },
      { href: 'timeline.html#summary', k: '最近总结', v: '「瓶颈在测试环境，不是 agent」', sub: '管家 1 小时前 · 点开看全文' },
    ],
    cols: {
      pending: [
        { t: '给看板卡片加因果链摘要', tags: [{ l: '可改文档' }], meta: '10:24 · 未派', from: '由管家从对话拆出' },
        { t: '隔离式项目：对话也拉不到时怎么办', tags: [{ l: '等澄清', cls: 'af-tag--warn' }], meta: '管家反问 · 缺「什么算跑通」' },
        { t: '收件箱：决策请求的超时时长默认值', tags: [{ l: '从 issue #42 导入' }], meta: '昨天 · 外部来源' },
        { t: '把失败原因写进卡面，而不是点开才看', why: { k: '没有可用设备', v: '3 台都不满足「node ≥ 20」，任务停在这里，没有随便找一台跑' }, meta: '已停 2 小时 · 需要你放宽约束或给设备装 node' },
      ],
      queued: [
        { t: '执行间接力：优先选跑过该项目的设备', tags: [{ l: '已选中 device-b', cls: 'af-tag--running' }], meta: '等同一绑定上的前一个任务跑完 · 预计 4 分钟' },
        { t: '管家总结的落点：项目摘要第 4 行', tags: [{ l: '等磁盘腾出 2.1 GB', cls: 'af-tag--warn' }], meta: 'device-c · 已用 91%，暂不派新任务' },
      ],
      running: [
        { t: '修复 git 面板在超大 diff 下卡死', beat: '18:42', beatWhat: '正在跑 cargo test', href: 'execution.html', tags: [{ l: 'claude', cls: 'af-tag--running' }], meta: 'device-b · 第 2 次执行' },
        { t: '为项目加主题色（纯视觉，不影响逻辑）', beat: '6:10', beatWhat: '正在改 tokens.css', href: 'execution.html', tags: [{ l: 'opencode', cls: 'af-tag--running' }], meta: 'device-a' },
        { t: '失败归因：把八种形态接进设备页', beat: '31:05', beatWhat: '', stall: true, href: 'execution.html', tags: [{ l: 'claude', cls: 'af-tag--running' }], meta: 'device-c · 3 分钟没有新动作' },
      ],
      review: [
        { t: '任务卡片显示排队时长', tags: [{ l: '判定条件已通过', cls: 'af-tag--done' }, { l: '需要你验收', cls: 'af-tag--review' }], meta: '+42 / −8 · 1 次提交 · 等你 40 分钟' },
      ],
      done: [
        { t: '项目实体与绑定固定目录', tags: [{ l: '已合入主干', cls: 'af-tag--done' }], meta: '今天 09:12 · 由你验收' },
        { t: '会话起点 base_commit 落到执行记录', tags: [{ l: '已合入主干', cls: 'af-tag--done' }], meta: '昨天 18:30 · 按预设规则自动验收' },
        { t: '收件箱两区（待决策 / 动态）', tags: [{ l: '已合入主干', cls: 'af-tag--done' }], meta: '昨天 11:05 · 由你验收' },
      ],
      failed: [
        { t: '批量重试时逐个检查任务级约束', why: { k: '没有可用设备', v: 'no_capable_device · 允许的 3 台都不满足 node ≥ 20' }, meta: '第 1 次 · 重试会换一台设备' },
        { t: '磁盘清理预演界面', why: { k: '磁盘空间不足', v: 'disk_exhausted · device-c 只剩 0.3 GB' }, meta: '第 2 次', tags: [{ l: '重试已用完', cls: 'af-tag--failed' }] },
      ],
    },
    butler: {
      title: '对这个项目说话',
      sub: 'AgentFleet 的管家 · 记忆存在项目库和 <span class="af-mono">.afleet/MEMORY.md</span>',
      badge: '2 条待确认',
    },
  },

  /* ── 个人笔记站：镜像式 ───────────────────────────────────────────────
     和别人不一样的坏法：上游 GitHub 已经动了，Hub 的镜像还没拉。
     所以这张看板必须能看出「我看到的不是最新的」—— 否则你以为在审最新代码。 */
  notes: {
    name: '个人笔记站',
    dot: 'dot-b',
    shape: '镜像式项目',
    shapeNote: '上游在 GitHub，Hub 只是镜像',
    bound: '1 台已绑定',
    filters: [
      { label: '只看笔记相关', cls: 'af-tag--review' },
    ],
    filterNote: '共 6 张卡 · 1 张被当前筛选隐藏',
    meters: [
      { href: 'task.html', k: '上次做到哪', v: '合入了「站内搜索」', sub: '昨天 20:15 · 由你验收' },
      { href: 'execution.html', k: '正在跑', v: '<span class="af-num">1</span> 个执行 · <span class="af-num">1</span> 台设备', sub: '已跑 4 分 12 秒' },
      { href: 'inbox.html', k: '等你决定', v: '<span class="af-num">1</span> 张卡待验收', sub: '上游有新提交，镜像落后 2 个' },
      { href: 'timeline.html#summary', k: '最近总结', v: '「镜像落后了，先拉再派活」', sub: '管家 3 小时前 · 点开看全文' },
    ],
    cols: {
      pending: [
        { t: '给文章加「最后修改」时间', tags: [{ l: '可改文档' }], meta: '昨天 · 未派' },
        { t: '修复 RSS 在某些阅读器里乱码', tags: [{ l: '等澄清', cls: 'af-tag--warn' }], meta: '管家反问 · 缺「哪个阅读器」' },
      ],
      queued: [
        { t: '把图片压缩接进构建流程', tags: [{ l: '等镜像同步', cls: 'af-tag--warn' }], meta: '上游已变，镜像落后 2 个 commit' },
      ],
      running: [
        { t: '站内搜索加中文分词', beat: '4:12', beatWhat: '正在跑构建', href: 'execution.html', tags: [{ l: 'claude', cls: 'af-tag--running' }], meta: 'device-a' },
      ],
      review: [
        { t: '文章页加目录侧栏', tags: [{ l: '判定条件已通过', cls: 'af-tag--done' }, { l: '需要你验收', cls: 'af-tag--review' }], meta: '+61 / −4 · 2 次提交 · 等你 2 小时' },
      ],
      done: [
        { t: '站内搜索', tags: [{ l: '已合入主干', cls: 'af-tag--done' }], meta: '昨天 20:15 · 由你验收' },
        { t: '深色模式跟随系统', tags: [{ l: '已合入主干', cls: 'af-tag--done' }], meta: '前天 14:02 · 由你验收' },
      ],
      failed: [
        { t: '自动把上游新提交拉进镜像', why: { k: '镜像有本地改动', v: 'mirror_dirty · 拉取会覆盖 1 个未提交的文件' }, meta: '第 1 次 · 需要你确认丢弃还是保留' },
      ],
    },
    butler: {
      title: '对这个项目说话',
      sub: '个人笔记站的管家 · 上游 <span class="af-mono">github.com/me/notes</span>',
      badge: '1 条要你确认',
    },
  },

  /* ── 内部指标台：隔离式 ───────────────────────────────────────────────
     关键差别：代码不落 Hub 磁盘，但这次执行的 diff 看得到 ——
     只是每次要向设备现取、看完即释放，而且 Hub 不验证那个 commit 的内容。
     这两条必须印在页面上，否则用户会以为它和中心式完全一样，
     点开一个迟迟不出结果的 diff 时不知道为什么慢。 */
  metrics: {
    name: '内部指标台',
    dot: 'dot-c',
    shape: '隔离式项目',
    shapeNote: '代码不落 Hub，diff 按需向设备取',
    bound: '1 台已绑定',
    isolation: true,
    filters: [
      { label: '只看 device-c', cls: 'af-tag--running' },
    ],
    filterNote: '共 8 张卡 · 隔离式项目的 diff 每次向设备现取，不留在 Hub',
    meters: [
      { href: 'task.html', k: '上次做到哪', v: '合入了「指标卡下钻」', sub: '3 天前 · 设备自报' },
      { href: 'execution.html', k: '正在跑', v: '<span class="af-num">1</span> 个执行 · <span class="af-num">1</span> 台设备', sub: 'device-c · 离线中，状态可能过期' },
      { href: 'inbox.html', k: '等你决定', v: '<span class="af-num">1</span> 张卡待验收 · <span class="af-num">1</span> 台设备离线', sub: 'diff 要向设备现取 · 设备离线，现在拿不到' },
      { href: 'timeline.html#summary', k: '最近总结', v: '「device-c 掉线，进度停在上次心跳」', sub: '管家 26 分钟前 · 点开看全文' },
    ],
    cols: {
      pending: [
        { t: '指标卡支持自定义时间范围', tags: [{ l: '可改文档' }], meta: '3 天前 · 未派' },
        { t: '加一个「本周对比上周」的视图', tags: [{ l: '等澄清', cls: 'af-tag--warn' }], meta: '管家反问 · 缺「对比哪几个指标」' },
      ],
      queued: [
        { t: '导出 CSV 时保留小数点原样', tags: [{ l: '等设备上线', cls: 'af-tag--warn' }], meta: 'device-c 离线 26 分钟，不派给它' },
      ],
      running: [
        { t: '修图表在超大数据下卡顿', beat: '12:30', beatWhat: '', stall: true, href: 'execution.html', tags: [{ l: 'claude', cls: 'af-tag--running' }], meta: 'device-c · 状态停在 12 分钟前的心跳' },
      ],
      review: [
        // 隔离式的待验收：diff 看得到，但它是向设备现取的、Hub 不长期存；
        // 而且 Hub 不验证那个 commit 的内容（见 project-and-snapshot.md §2.1.1）。
        // 所以卡面上要说清「你看到的是设备给的，不是 Hub 核对过的」。
        { t: '指标卡下钻到明细', tags: [{ l: '改动清单已取回', cls: 'af-tag--done' }, { l: 'Hub 未核对 commit', cls: 'af-tag--warn' }], meta: '3 个文件 +38 / −6 · 向 device-c 现取的 diff，不留存' },
      ],
      done: [
        { t: '指标卡下钻', tags: [{ l: '已合入主干', cls: 'af-tag--done' }], meta: '3 天前 · commit 由 device-c 报来，Hub 不验证内容' },
      ],
      failed: [
        { t: '指标数据每小时同步一次', why: { k: '设备离线', v: 'device_offline · device-c 26 分钟没有心跳，最后一次同步停在 11:04' }, meta: '第 3 次', tags: [{ l: '已按规则暂停重试', cls: 'af-tag--failed' }] },
      ],
    },
    butler: {
      title: '对这个项目说话',
      sub: '内部指标台的管家 · 隔离式，代码不出 device-c',
      badge: '1 条要你确认',
    },
  },
};

const STATE_NAMES = {
  pending: '待办', queued: '排队中', running: '执行中',
  review: '待验收', done: '已完成', failed: '失败',
};

/* 卡片渲染：把数据变成和原来一模一样的结构。
   刻意不用 innerHTML 拼整张卡 —— 标题里有用户输入的东西，
   虽然这是原型，但别养成坏习惯。 */
function cardEl(c) {
  const a = document.createElement('a');
  a.className = 'af-card';
  a.href = c.href || 'task.html';

  const title = document.createElement('span');
  title.className = 'af-card__title';
  title.textContent = c.t;
  a.appendChild(title);

  if (c.beat) {
    const beat = document.createElement('span');
    beat.className = 'af-beat';
    const dot = document.createElement('span');
    dot.className = 'af-beat__dot' + (c.stall ? ' af-beat__dot--stall' : '');
    dot.setAttribute('aria-hidden', 'true');
    beat.appendChild(dot);
    const time = document.createElement('span');
    time.textContent = c.beat;
    beat.appendChild(time);
    if (c.beatWhat) {
      const sep = document.createElement('span');
      sep.className = 'af-sep';
      sep.textContent = '·';
      beat.appendChild(sep);
      const what = document.createElement('span');
      what.textContent = c.beatWhat;
      beat.appendChild(what);
    }
    if (c.stall) {
      const tag = document.createElement('span');
      tag.className = 'af-tag af-tag--warn';
      tag.textContent = '可能卡住';
      beat.appendChild(tag);
    }
    a.appendChild(beat);
  }

  if (c.why) {
    const why = document.createElement('span');
    why.className = 'af-card__why';
    const k = document.createElement('b');
    k.textContent = c.why.k;
    why.appendChild(k);
    const v = document.createElement('span');
    v.textContent = c.why.v;
    why.appendChild(v);
    a.appendChild(why);
  }

  if (c.tags && c.tags.length) {
    const meta = document.createElement('span');
    meta.className = 'af-card__meta';
    for (const t of c.tags) {
      const tag = document.createElement('span');
      tag.className = 'af-tag' + (t.cls ? ' ' + t.cls : '');
      tag.textContent = t.l;
      meta.appendChild(tag);
    }
    a.appendChild(meta);
  }

  if (c.meta) {
    const meta = document.createElement('span');
    meta.className = 'af-card__meta';
    meta.textContent = c.meta;
    a.appendChild(meta);
  }
  return a;
}

function renderBoard(id) {
  const b = BOARDS[id] || BOARDS.afleet;

  /* 页头：项目名 + 形态 + 绑定数。形态那一行要能看出「Hub 能看到什么」，
     因为看板能给你看什么是由它决定的。 */
  document.title = `看板 · ${b.name}`;
  document.getElementById('b-switch-name').textContent = b.name;
  document.getElementById('b-shape').textContent = b.shape;
  document.getElementById('b-bound').textContent = b.bound;

  /* 隔离式项目要在页头说清「什么不在 Hub 上」。
     ---------------------------------------------------------------------
     这里最容易写错：把「代码不上传」写成「你看不到代码」。
     那是两回事，而且后者会把隔离式做成黑盒 —— 黑盒没人愿意用。
     正确的界线按【存储位置】分，不按「能不能看见」分：

       源码树 / packfile / 完整历史  → 只在设备上，Hub 不落盘
       本次执行的对话、改动清单、diff → Hub 可读，但【按需向设备拉、看完即释放】
       环境声明                      → 只报哈希不报内容

     所以页头要说的是「看得见，只是每次要走设备」，不是「看不见」。 */
  const warnBox = document.getElementById('b-isolation');
  if (b.isolation) {
    warnBox.hidden = false;
    warnBox.querySelector('.af-callout__k').textContent = '代码不落 Hub，但你都看得见';
    warnBox.querySelector('.af-2xs').textContent =
      '隔离式项目：源码和完整历史只在 device-c 上，Hub 不存、也不随 git 上传。'
      + '但本次执行的对话、改动清单和 diff 都能看 —— 按需向设备现取，看完即释放。'
      + '区别只有两条：每次查看要多等一会儿，以及那个 commit 的内容 Hub 不会替你核对（由设备承诺）。';
  } else {
    warnBox.hidden = true;
  }

  /* 摘要四行 */
  const meters = document.getElementById('b-meters');
  meters.textContent = '';
  for (const m of b.meters) {
    const a = document.createElement('a');
    a.className = 'af-meter' + (m.action ? ' af-meter--action' : '');
    a.href = m.href;
    const k = document.createElement('span');
    k.className = 'af-meter__k';
    k.textContent = m.k;
    a.appendChild(k);
    const v = document.createElement('span');
    v.className = 'af-meter__v' + (m.action ? ' af-meter__v--warn' : '');
    v.innerHTML = m.v;   /* 这里的值是本文件里的字面量，不是用户输入 */
    a.appendChild(v);
    const sub = document.createElement('span');
    sub.className = 'af-meter__sub';
    sub.textContent = m.sub;
    a.appendChild(sub);
    meters.appendChild(a);
  }

  /* 筛选条 */
  const filters = document.getElementById('filter');
  filters.textContent = '';
  if (b.filters.length) {
    const k = document.createElement('span');
    k.className = 'af-filters__k';
    k.textContent = '生效中的筛选';
    filters.appendChild(k);
    for (const f of b.filters) {
      const t = document.createElement('span');
      t.className = 'af-tag' + (f.cls ? ' ' + f.cls : '');
      t.textContent = f.label;
      filters.appendChild(t);
    }
    const clear = document.createElement('a');
    clear.href = 'board.html?project=' + id;
    clear.className = 'af-2xs af-dim';
    clear.textContent = '清除';
    filters.appendChild(clear);
  }
  const spacer = document.createElement('span');
  spacer.className = 'af-spacer';
  filters.appendChild(spacer);
  const note = document.createElement('span');
  note.className = 'af-2xs af-mute';
  note.textContent = b.filterNote;
  filters.appendChild(note);

  /* 六列 */
  for (const [state, name] of Object.entries(STATE_NAMES)) {
    const cards = b.cols[state] || [];
    const body = document.getElementById('bcol-' + state);
    body.textContent = '';
    for (const c of cards) body.appendChild(cardEl(c));
    const count = document.getElementById('bcount-' + state);
    count.textContent = String(cards.length);
    count.setAttribute('aria-label', cards.length + ' 张');
    /* 空列不隐藏：空着的列本身是信息（这个项目现在没有失败），
       但要说清是「没有」还是「被筛掉了」 */
    const col = document.getElementById('bcol-' + state).closest('.af-col');
    col.classList.toggle('af-col--empty', cards.length === 0);
    if (cards.length === 0) {
      body.appendChild(el(''));  /* 占位，样式负责写「这一列是空的」 */
    }
  }

  /* 管家面板：换项目就换管家。它不是全局客服，是项目的那一个。 */
  document.getElementById('b-butler-title').textContent = b.butler.title;
  document.getElementById('b-butler-sub').innerHTML = b.butler.sub;
  document.getElementById('b-butler-badge').textContent = b.butler.badge;
  document.getElementById('b-butler-fab').setAttribute(
    'aria-label', '打开管家，' + b.butler.badge);

  /* 切换器：把当前项目标出来，并把三个项目都填进下拉 */
  renderSwitcher(id);
}

function renderSwitcher(currentId) {
  const btn = document.getElementById('b-switch-btn');
  const menu = document.getElementById('b-switch-menu');
  const cur = BOARDS[currentId];
  document.getElementById('b-switch-dot').className = 'af-dot ' + cur.dot;
  document.getElementById('b-switch-name').textContent = cur.name;

  menu.textContent = '';
  for (const [id, b] of Object.entries(BOARDS)) {
    const a = document.createElement('a');
    a.className = 'af-switch__item';
    a.href = withTheme('board.html?project=' + id);
    if (id === currentId) a.setAttribute('aria-current', 'true');
    a.innerHTML = `
      <span class="af-dot ${b.dot}" aria-hidden="true"></span>
      <span class="af-switch__label">${b.name}</span>
      <span class="af-switch__shape">${b.shape}</span>
    `;
    menu.appendChild(a);
  }
  /* 建新项目也放这里：切换项目和新建项目是同一个念头「我要去看别的」 */
  const add = document.createElement('a');
  add.className = 'af-switch__item af-switch__item--add';
  add.href = withTheme('project-new.html');
  add.textContent = '+ 新建项目';
  menu.appendChild(add);
}

/* 和 shell.js 里的同名函数行为一致：换页时把配色带上。
   shell.js 已经定义了 window 级的 href/withTheme，但那是模块内的 const，
   这里不能假设它一定先执行完，所以自己算一遍。 */
function withTheme(h) {
  const t = new URLSearchParams(location.search).get('theme');
  if (!t) return h;
  return h + (h.includes('?') ? '&' : '?') + 'theme=' + t;
}

document.addEventListener('DOMContentLoaded', () => {
  const id = new URLSearchParams(location.search).get('project') || 'afleet';
  renderBoard(BOARDS[id] ? id : 'afleet');

  /* 下拉开合。用 data-open 而不是 :target —— 下拉不该改地址栏，
     否则后退键会退回「下拉关着的状态」，用户会以为页面没变。 */
  const switcher = document.getElementById('b-switch');
  const btn = document.getElementById('b-switch-btn');
  const close = () => {
    switcher.dataset.open = 'false';
    btn.setAttribute('aria-expanded', 'false');
  };
  btn.addEventListener('click', (e) => {
    e.preventDefault();
    const open = switcher.dataset.open === 'true';
    switcher.dataset.open = open ? 'false' : 'true';
    btn.setAttribute('aria-expanded', String(!open));
  });
  addEventListener('pointerdown', (e) => {
    if (switcher.dataset.open !== 'true') return;
    if (switcher.contains(e.target)) return;
    close();
  });
  addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && switcher.dataset.open === 'true') { close(); btn.focus(); }
  });

  /* Alt+↑/↓ 切换项目：不用打开下拉也能换。
     用 Alt 而不是单按方向键 —— 单按方向键会和页面滚动、看板横向滚动打架。

     外面包了 try：换页失败（被拦截、非浏览器环境）时不能连累整段脚本，
     这和之前 matchMedia 那次的教训是同一类。 */
  addEventListener('keydown', (e) => {
    try {
      if (!e.altKey || (e.key !== 'ArrowUp' && e.key !== 'ArrowDown')) return;
      if (e.ctrlKey || e.metaKey || e.shiftKey) return;
      if (!document.getElementById('b-switch')) return;
      const ids = Object.keys(BOARDS);
      const at = ids.indexOf(id);
      const step = e.key === 'ArrowDown' ? 1 : ids.length - 1;
      const next = ids[(at + step) % ids.length];
      e.preventDefault();
      location.href = withTheme('board.html?project=' + next);
    } catch (err) {
      /* 键盘快捷方式是锦上添花，坏了也不能影响页面 */
    }
  });
});
