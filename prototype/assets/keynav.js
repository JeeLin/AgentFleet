/* ===========================================================================
   keynav.js —— 焦点与键位的唯一真相源（规范 §4）
   ---------------------------------------------------------------------------
   这个文件承担三件事，且只有它承担：

     ① KEYS  —— 全局键位表。`?` 面板**由它渲染**，所以「文档写的键位」
                与「按下会发生的键位」不可能漂移。
     ② Focus —— 焦点栈 + inert 管理。任何浮层的开与关都走这里，
                不存在页面自己 focus() 一下的散兵游勇。
     ③ Guard —— 表里有 act 就必须有 handler；表里没有的 act 注册不进来。

   ── 上一版的三个根因（这一版逐个拆掉）────────────────────────────────
   ① 31 个键位里 15 个没有 handler。根因是「键位表」与「实现」是两份
      东西，写表的人不需要对实现负责。现在 `AF.register` 检查 act 在表里，
      `AF.unimplemented()` 在控制台列出表里有、没人实现的项。
   ② `KEYS` 是死变量——6 个页面宣传 `?` 面板，而它从未被读取。现在
      `AF.renderHelp()` 就是这张表的消费者。
   ③ `trapFocus` 缓存 focusables 快照，模态 innerHTML 重写后过期，
      Tab 圈住的是上一批按钮。现在每次 Tab 都重新查。

   ── 焦点所有权的唯一模型 ────────────────────────────────────────────
   焦点归「最内层活动容器」。容器打开时，背后内容全部 inert
   （浏览器原生语义：焦点、指针事件、辅助树三处一起管，
   比手工 aria-hidden + tabindex 可靠得多）。
   容器关闭时，焦点还给打开它的那个元素——**每条关闭路径都走 pop()**，
   所以 Esc、点遮罩、点关闭按钮三条路的行为一致。
   =========================================================================== */
(function () {
  'use strict';

  var AF = (window.AF = window.AF || {});
  var win = window;

  /* =========================================================================
     §1 全局键位表
     -------------------------------------------------------------------------
     每一项：k（键位描述）· act（动作名，必须唯一）· scope（生效范围）
       scope 'global'  任何页面生效，输入区内不抢单键
       scope 'list'    列表/树容器内生效
       scope 'session' 只在会话工作台生效
       scope 'panel'   只在某个面板获得焦点时生效
     ======================================================================= */
  var KEYS = [
    { group: '全局', keys: [
      { k: '?',        act: 'help:toggle',   desc: '打开 / 关闭键位帮助' },
      { k: '/',        act: 'search:focus',  desc: '聚焦搜索框' },
      { k: 'Esc',      act: 'layer:escape',  desc: '关闭最上层浮层；无浮层时取消选中' },
      { k: 'Tab',      act: 'focus:next',    desc: '焦点前进（有浮层时在浮层内循环）' },
      { k: '⇧Tab',     act: 'focus:prev',    desc: '焦点后退（有浮层时在浮层内循环）' },
      { k: '⌃B',        act: 'rail:toggle',   desc: '折叠 / 展开侧栏' }
    ]},
    { group: '跳转', scope: 'global', keys: [
      { k: 'g s', act: 'nav:sessions',    desc: '会话列表' },
      { k: 'g n', act: 'nav:new-session', desc: '新建会话' },
      { k: 'g d', act: 'nav:devices',     desc: '设备列表' },
      { k: 'g a', act: 'nav:agents',      desc: 'Agent 目录' },
      { k: 'g ,', act: 'nav:settings',    desc: '设置' }
    ]},
    { group: '列表 / 树', scope: 'list', keys: [
      { k: '↑ ↓', act: 'list:move',   desc: '移动焦点行（j / k 同）' },
      { k: '⏎',   act: 'list:enter',  desc: '进入选中项' },
      { k: 'Space', act: 'list:toggle', desc: '展开 / 收起目录，或多选切换' }
    ]},
    { group: '会话工作台', scope: 'session', keys: [
      { k: 'o',  act: 'panel:open',  desc: '打开一个面板（终端 / 文件）' },
      { k: '[',  act: 'panel:prev',  desc: '切换到上一个已打开的面板' },
      { k: ']',  act: 'panel:next',  desc: '切换到下一个已打开的面板' },
      { k: '\\', act: 'dock:toggle', desc: '打开 / 收起面板坞' },
      { k: 't',  act: 'traj:toggle', desc: '打开 / 关闭轨迹抽屉' },
      { k: 'g g', act: 'traj:toggle', desc: '打开 / 关闭轨迹抽屉（g 序列）', alias: true }
    ]},
    { group: '终端面板', scope: 'panel', keys: [
      { k: '⌃L', act: 'term:clear', desc: '清屏' }
    ]},
    { group: '文件面板', scope: 'panel', keys: [
      { k: '⌃S',        act: 'file:save', desc: '保存当前文件' },
      { k: 'Backspace', act: 'file:up',   desc: '回到上一级目录' }
    ]}
  ];

  /* g 序列的目标：g 之后 800ms 内的第二个键决定去向 */
  var PREFIX = {
    s: 'nav:sessions', n: 'nav:new-session', d: 'nav:devices',
    a: 'nav:agents', ',': 'nav:settings', g: 'traj:toggle'
  };
  var PREFIX_MS = 800;

  /* 反向索引：act → 定义。注册时用它校验「表里有没有这个 act」。 */
  var ACTS = {};
  KEYS.forEach(function (g) {
    g.keys.forEach(function (k) {
      if (ACTS[k.act]) {
        /* alias: 同一个 act 有两个键位是允许的（g g 与 t 同一动作），
           但不许两个 act 共用一个键位描述。 */
        if (ACTS[k.act].def.k !== k.k) {
          throw new Error('keynav: act "' + k.act + '" 有两个不同键位描述');
        }
        return;
      }
      ACTS[k.act] = { def: k, group: g.group, scope: g.scope || 'global' };
    });
  });

  /* =========================================================================
     §2 handler 注册与派发
     -------------------------------------------------------------------------
     handler 返回值决定是否吞掉这个键：true = 吞，false / undefined = 不吞。
     「不吞」很重要——它让没实现的动作把键还给浏览器，
     而不是上一版那种「按了没反应但键也被抢走」。
     ======================================================================= */
  var handlers = {};

  AF.register = function (act, fn) {
    if (!ACTS[act]) {
      throw new Error(
        'keynav: act "' + act + '" 不在 KEYS 表里。\n' +
        '  先把它加进 keynav.js 的 KEYS（用户要能在 `?` 里看到它），\n' +
        '  再注册 handler。反过来不许：不许有用户找不到的隐藏键位。'
      );
    }
    if (handlers[act]) {
      throw new Error('keynav: act "' + act + '" 被注册了两次 —— 一个动作只能有一个主人');
    }
    handlers[act] = fn;
    return AF;
  };

  /* 内部注册：不查表（本文件的 Esc/Tab 在自己注册自己）。 */
  function register(act, fn) { handlers[act] = fn; }

  AF.dispatch = function (act, e, data) {
    var fn = handlers[act];
    if (!fn) return false;
    var r = fn(e, data);
    if (r === true && e && e.cancelable) e.preventDefault();
    return r === true;
  };

  AF.hasHandler = function (act) { return !!handlers[act]; };

  /* 表里有、但没人实现的 act。开发期在控制台吵，而不是等用户按到死键。 */
  AF.unimplemented = function () {
    return Object.keys(ACTS).filter(function (a) { return !handlers[a]; });
  };
  AF.keysTable = function () { return KEYS; };

  /* -------------------------------------------------------------------------
     自己的动作自己注册。Esc / Tab 是这个文件的职责（焦点栈归它管），
     所以 handler 也在这里——**不许出现「表里有、监听器里直接处理」的
     第三种路径**。上一版 15 个死键的根因正是这条捷径。
     ----------------------------------------------------------------------- */
  register('layer:escape', function () {
    if (stack.length) {
      AF.focusPop(stack[stack.length - 1].container);
      return true;
    }
    /* 没有浮层时，Esc 取消列表选中 */
    var list = listOf(document.activeElement);
    if (list) {
      AF.rowsIn(list).forEach(function (r) { r.setAttribute('aria-selected', 'false'); });
      return true;
    }
    return false;
  });

  register('focus:next', function (e) {
    if (!stack.length) return false;
    trapTab(e);
    return true;
  });

  register('focus:prev', function (e) {
    if (!stack.length) return false;
    trapTab(e);
    return true;
  });

  /* =========================================================================
     §3 焦点栈
     ======================================================================= */
  var stack = [];

  function inertAll(except, on) {
    /* 把 except 之外的所有直接子节点设为 inert。原生 inert 同时管
       焦点、指针事件与辅助树——比 aria-hidden + tabindex 手工管三处可靠。 */
    [].forEach.call(document.body.children, function (el) {
      if (el === except || el.contains(except)) return;
      el.inert = on;
      if (on) el.setAttribute('aria-hidden', 'true');
      else el.removeAttribute('aria-hidden');
    });
  }

  AF.focusPush = function (container, opts) {
    opts = opts || {};
    stack.push({ container: container, opener: opts.opener || document.activeElement });
    inertAll(container, true);
    var first = container.querySelector(
      '[data-autofocus], button, input, select, textarea, [tabindex]:not([tabindex="-1"])'
    );
    if (first) first.focus();
    else if (container.hasAttribute('tabindex')) container.focus();
    return container;
  };

  AF.focusPop = function (container) {
    var i = stack.length - 1;
    while (i >= 0 && stack[i].container !== container) i--;
    if (i < 0) return false;
    var layer = stack[i];
    stack.splice(i, 1);
    inertAll(layer.container, false);
    /* 还焦点。opener 可能已经被移除（点它的那一行刚被删了），
       这时退到 main 而不是留下悬空引用。 */
    if (layer.opener && document.contains(layer.opener) && typeof layer.opener.focus === 'function') {
      layer.opener.focus();
    } else {
      var main = document.querySelector('main[tabindex="-1"]');
      if (main) main.focus();
    }
    return true;
  };

  AF.focusDepth = function () { return stack.length; };

  /* =========================================================================
     §4 焦点陷阱（动态版）
     -------------------------------------------------------------------------
     每次 Tab 都重新查焦点元素——上一版的快照在模态 innerHTML 重写后失效。
     ======================================================================= */
  var FOCUSABLE = 'a[href], button:not([disabled]), input:not([disabled]), ' +
                  'select:not([disabled]), textarea:not([disabled]), ' +
                  '[tabindex]:not([tabindex="-1"])';

  AF.focusablesIn = function (root) {
    return [].filter.call(root.querySelectorAll(FOCUSABLE), function (el) {
      return !el.closest('[hidden]') && el.offsetParent !== null;
    });
  };

  function trapTab(e) {
    var top = stack[stack.length - 1].container;
    var items = AF.focusablesIn(top);
    if (!items.length) { e.preventDefault(); return; }
    var first = items[0], last = items[items.length - 1], cur = document.activeElement;
    if (!top.contains(cur)) { e.preventDefault(); first.focus(); return; }
    if (e.shiftKey && cur === first) { e.preventDefault(); last.focus(); }
    else if (!e.shiftKey && cur === last) { e.preventDefault(); first.focus(); }
  }

  /* =========================================================================
     §5 列表 / 树导航
     -------------------------------------------------------------------------
     容器是一个 tab stop（[data-role] 带 tabindex="0"），行是 tabindex="-1"。
     这是标准 listbox/grid 模式：Tab 进容器，方向键在行间走。
     上一版用 div + tabindex=-1 + 全局 Enter 代理，Tab 根本进不去。

     行是 [data-row]。选中态写 aria-selected（辅助技术可读），
     不只是加个背景色。
     ======================================================================= */
  function listOf(el) {
    return el && el.closest ? el.closest('[data-role="list"], [data-role="tree"]') : null;
  }
  AF.listOf = listOf;

  AF.rowsIn = function (list) {
    return [].filter.call(list.querySelectorAll('[data-row]'), function (r) {
      return !r.closest('[hidden]') && r.offsetParent !== null;
    });
  };

  function selectRow(row, focus) {
    var list = listOf(row);
    if (!list) return;
    var rows = AF.rowsIn(list);
    rows.forEach(function (r) {
      r.setAttribute('aria-selected', r === row ? 'true' : 'false');
    });
    if (focus !== false) row.focus();
  }

  AF.selectRow = selectRow;

  AF.currentRow = function () {
    var list = listOf(document.activeElement);
    if (!list) return null;
    return AF.rowsIn(list).filter(function (r) {
      return r.getAttribute('aria-selected') === 'true';
    })[0] || null;
  };

  function moveSelection(delta) {
    var list = listOf(document.activeElement);
    if (!list) return false;
    var rows = AF.rowsIn(list);
    if (!rows.length) return false;
    var cur = rows.indexOf(document.activeElement);
    var next;
    if (cur < 0) next = delta > 0 ? 0 : rows.length - 1;
    else next = Math.max(0, Math.min(rows.length - 1, cur + delta));
    selectRow(rows[next]);
    return true;
  }

  /* 容器自身获得焦点时，把焦点交给当前行（或第一行）——否则方向键失灵 */
  document.addEventListener('focusin', function (e) {
    var row = e.target.closest && e.target.closest('[data-row]');
    if (row) { selectRow(row, false); return; }
    var list = e.target.closest && e.target.closest('[data-role="list"], [data-role="tree"]');
    if (list && list === document.activeElement) {
      var rows = AF.rowsIn(list);
      var sel = rows.filter(function (r) { return r.getAttribute('aria-selected') === 'true'; })[0];
      if (rows.length) (sel || rows[0]).focus();
    }
  });

  document.addEventListener('pointerdown', function (e) {
    var row = e.target.closest && e.target.closest('[data-row]');
    if (row && row.tabIndex === -1) { row.focus(); selectRow(row, false); }
  });

  AF.moveSelection = moveSelection;

  /* =========================================================================
     §6 键位帮助面板
     -------------------------------------------------------------------------
     内容由 KEYS 渲染 —— 这是「表即文档」的落点。
     上一版 components.html 声称面板由 KEYS 渲染，但 KEYS 从未被读取，
     于是 6 个页面宣传的 `?` 是个死键。这一版它就是这张表的消费者。
     ======================================================================= */
  function esc(s) {
    return String(s).replace(/[&<>\"]/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c];
    });
  }
  AF.esc = esc;

  var SCOPE_LABEL = { list: '列表', session: '会话工作台', panel: '面板' };

  function renderHelp() {
    return KEYS.map(function (g) {
      var scope = g.scope === 'global' ? '' :
        '<span class="af-t-label">' + (SCOPE_LABEL[g.scope] || g.scope) + '</span>';
      var rows = g.keys.map(function (k) {
        var dead = handlers[k.act] ? '' : ' data-unimplemented="true"';
        var alias = k.alias ? ' class="help__alias"' : '';
        return '<tr' + dead + alias + '><td class="kbd-cell"><kbd>' + esc(k.k) + '</kbd></td>' +
               '<td>' + esc(k.desc) + '</td></tr>';
      }).join('');
      return '<div class="help__group"><div class="help__gh">' + esc(g.group) + scope + '</div>' +
             '<table class="help__t">' + rows + '</table></div>';
    }).join('');
  }

  AF.renderHelp = renderHelp;

  /* =========================================================================
     §7 输入区判定
     -------------------------------------------------------------------------
     单键在输入区内一律不抢——这是「键盘优先」与「能打字」的分界。
     修饰键（⌃B 等）在输入区内也生效，因为用户可能在终端里按它。
     ======================================================================= */
  function isTyping(el) {
    if (!el) return false;
    var t = el.tagName;
    return t === 'INPUT' || t === 'TEXTAREA' || t === 'SELECT' || el.isContentEditable;
  }

  /* =========================================================================
     §8 全局监听
     -------------------------------------------------------------------------
     顺序有讲究：修饰键先判（输入区内也放行），再判 Esc / Tab，
     然后是 g 序列，最后判单键。
     ======================================================================= */
  var pendingG = 0;

  document.addEventListener('keydown', function (e) {
    if (e.isComposing) return;          /* 中文拼音选字期间不抢键 */
    if (e.metaKey && e.key !== 'Meta') return; /* 让位给系统快捷键 */

    var typing = isTyping(e.target);
    var mod = e.ctrlKey || e.metaKey;

    /* ── g 序列的第二个键 ── */
    if (pendingG) {
      var was = pendingG;
      pendingG = 0;
      if (Date.now() - was < PREFIX_MS && !typing) {
        var act = PREFIX[e.key];
        if (act && AF.dispatch(act, e)) return;
      }
      /* 没配上就不吞键：用户只是在打一段以 g 开头的字 */
    }

    /* ── Esc / Tab：交给自己的 handler（本文件的职责） ── */
    if (e.key === 'Escape') { AF.dispatch('layer:escape', e); return; }
    if (e.key === 'Tab' && stack.length) { AF.dispatch(e.shiftKey ? 'focus:prev' : 'focus:next', e); return; }

    /* ── ⌃B：折叠侧栏（输入区内也放行） ── */
    if (mod && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'b') {
      AF.dispatch('rail:toggle', e); return;
    }

    /* 以下单键在输入区内不抢 */
    if (typing) return;

    /* ── g：开启序列窗口 ── */
    if (e.key === 'g') { pendingG = Date.now(); return; }

    /* ── ? ── */
    if (e.key === '?') { AF.dispatch('help:toggle', e); return; }

    /* ── /：聚焦搜索 ── */
    if (e.key === '/') { AF.dispatch('search:focus', e); return; }

    /* ── 列表导航 ── */
    if (e.key === 'ArrowDown' || e.key === 'j') { AF.dispatch('list:move', e, 1); return; }
    if (e.key === 'ArrowUp'   || e.key === 'k') { AF.dispatch('list:move', e, -1); return; }
    if (e.key === 'Enter' && listOf(document.activeElement)) { AF.dispatch('list:enter', e); return; }
    if (e.key === ' ') {
      /* Space 在有按钮聚焦时是「按下按钮」，别抢 */
      var ae = document.activeElement;
      if (ae && (ae.tagName === 'BUTTON' || ae.tagName === 'A' || ae.isContentEditable)) return;
      AF.dispatch('list:toggle', e); return;
    }

    /* ── 会话工作台与面板（scope 由是否注册了 handler 决定：
           没注册 = 本页不适用 = 键还给浏览器，而不是死键） ── */
    if (e.key === 'o')  { AF.dispatch('panel:open', e); return; }
    if (e.key === '[')  { AF.dispatch('panel:prev', e); return; }
    if (e.key === ']')  { AF.dispatch('panel:next', e); return; }
    if (e.key === '\\') { AF.dispatch('dock:toggle', e); return; }
    if (e.key === 't')  { AF.dispatch('traj:toggle', e); return; }

    /* ── 面板内 ── */
    if (mod && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'l') { AF.dispatch('term:clear', e); return; }
    if (mod && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 's') { AF.dispatch('file:save', e); return; }
    if (e.key === 'Backspace' && listOf(document.activeElement)) { AF.dispatch('file:up', e); return; }
  });

  /* =========================================================================
     §9 首焦点：页面跳转后的落点（D1）
     -------------------------------------------------------------------------
     上一版 12 个页面 0 个 autofocus、0 个 main[tabindex=-1]：
     跳转后焦点回到 <body>，屏幕阅读器不播报新页面。这里由外壳统一做。
     ======================================================================= */
  AF.focusMain = function () {
    var main = document.querySelector('main[tabindex="-1"]');
    if (!main) return;
    var h1 = main.querySelector('h1');
    if (h1 && !h1.hasAttribute('tabindex')) h1.setAttribute('tabindex', '-1');
    main.focus();
  };

  document.addEventListener('DOMContentLoaded', function () {
    AF.focusMain();
    var dead = AF.unimplemented();
    if (dead.length && win.console) {
      console.warn('[keynav] 表里有键位但没有 handler：', dead.join(', '));
    }
  });

})();
