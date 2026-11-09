#!/usr/bin/env bash
# 本仓库的 grep 门禁。新增规则按 C 序号追加。
#
# 用法：scripts/check.sh [--strict-anchors]
#
#   --strict-anchors  把「里程碑文档里待迁移的裸行号锚点」也算失败。
#                     默认只报不红——那些锚点归主线迁移（见 M1 里程碑 Bugs 表 910 条）。
set -uo pipefail
cd "$(dirname "$0")/.."

STRICT_ANCHORS=0
for arg in "$@"; do
  case "$arg" in
    --strict-anchors) STRICT_ANCHORS=1 ;;
    -h | --help)
      awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
      exit 0 ;;
    *)
      echo "未知参数：$arg（--help 看用法）" >&2
      exit 2 ;;
  esac
done

fail=0
ok() { echo "  ✅ $1"; }
bad() { echo "  ❌ $1"; fail=1; }

echo "== C1 子 crate 不得手写依赖版本号（必须收敛到 [workspace.dependencies]）=="
# 只看依赖表（`[dependencies]` / `[dev-dependencies]` / `[build-dependencies]` /
# `[target.*.dependencies]`），两种写法都要拦：
#   裸字符串    `tokio = "1"`
#   内联表      `serde = { version = "1.0", features = [...] }`
# 收敛后的正确写法 `xxx.workspace = true` 与 `xxx = { workspace = true }` 放行。
hits=$(awk '
  FNR == 1 { sect = "" }
  /^[[:space:]]*\[/ { sect = $0; next }
  {
    if (sect !~ /dependencies/) next
    if (match($0, /^[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*=/)) {
      p = RSTART + RLENGTH
      val = substr($0, p)
      if (val ~ /^[[:space:]]*"/) {
        v = val
        sub(/^[[:space:]]*"/, "", v)
        if (v ~ /^[0-9]/) print FILENAME ":" FNR ":" $0
      } else if (val ~ /^[[:space:]]*\{/) {
        if (val ~ /version[[:space:]]*=[[:space:]]*"[0-9]/) print FILENAME ":" FNR ":" $0
      }
    }
  }
' crates/*/Cargo.toml)
if [[ -z "$hits" ]]; then ok "无手写版本号"; else bad "子 crate 出现手写版本号："; echo "$hits" | sed 's/^/      /'; fi

echo "== C2 设备二进制不得被称作 agent（约束 2）=="
# 与 C3 同口径排除审查报告目录：报告为了证明「某模式零命中」会把正则本身写进正文，
# 那不是规格在把设备叫 agent。目录名带里程碑号（`M0-reports/` / `M1-reports/`），
# 所以排除写成 `-reports/` 通配，不写死当前里程碑名。
hits=$(grep -rnE 'Dockerfile\.agent|agentfleet-agent|agent-binaries|docker-agent|agent 镜像' --include='*.md' docs/ .mdflow/ 2>/dev/null \
  | grep -v -- '-reports/' || true)
if [[ -z "$hits" ]]; then ok "命名合规"; else bad "出现把设备二进制叫 agent 的用法："; echo "$hits" | sed 's/^/      /'; fi

echo "== C3 文档锚点必须有效（A 轨 §N.N 解析到小节标题；B 轨 file.md:N 指向稳定锚点行）=="
CHECK_STRICT_ANCHORS=$STRICT_ANCHORS python3 - <<'PYEOF'
import re, os, sys, glob

STRICT = os.environ.get('CHECK_STRICT_ANCHORS') == '1'

# ---- 扫描范围 -------------------------------------------------------------
# 里程碑文档：.mdflow/milestones/*.md（不含 -reports/ 下的审查报告）
MILESTONE = [p for p in sorted(glob.glob('.mdflow/milestones/*.md'))
             if not os.path.basename(p).startswith('-')]
# 仓库自有文档：对外文档 + 根文件 + 夹具说明（这些的锚点归本仓库自己维护）
OWN = sorted(glob.glob('docs/**/*.md', recursive=True)) + [
    'README.md', 'AGENTS.md', 'CHANGELOG.md', 'scripts/e2e-m1/README.md',
    'scripts/e2e-m2/README.md']
OWN = [p for p in OWN if os.path.exists(p)]

# ---- 别名表 ---------------------------------------------------------------
# 规范允许前缀省略（「设计规范」= design-system.md、「protocol.md」=
# docs/architecture/protocol.md），故此处做前缀匹配而非全等。
#
# `README` 指**仓库根**那份：仓库里对架构目录那份的引用本来就都写全路径
# （`docs/architecture/README.md`），只有根那份常用裸名。
ALIASES = {
    'design-system': 'docs/design-system.md',
    'protocol': 'docs/architecture/protocol.md',
    'PRODUCT': 'docs/PRODUCT.md',
    'DEVELOPMENT': 'docs/DEVELOPMENT.md',
    'DECISIONS': 'docs/DECISIONS.md',
    'BUGS': 'docs/BUGS.md',
    'AGENTS': 'AGENTS.md',
    'README': 'README.md',
    'security': 'docs/architecture/security-and-lifecycle.md',
    'scheduling': 'docs/architecture/scheduling.md',
    'session-and-trajectory': 'docs/architecture/session-and-trajectory.md',
    'task-continuity': 'docs/architecture/task-continuity.md',
    'workspace-and-sync': 'docs/architecture/workspace-and-sync.md',
    'ai-gateway': 'docs/architecture/ai-gateway.md',
    'inference': 'docs/architecture/inference.md',
}
# 中文简称。值是 (路径, 匹配方式)：
#   'bare'     正文里可以裸匹配（`协议`、`设计规范`）；
#   'anchored' 必须带「:行号」才认（`产品:92`）——裸词「产品」「开发」「决策」
#              是正文里的普通词（开发环境、产品决策），裸匹配会把散文误判成
#              文档提及，进而让 § 锚点解析到错的文档。
ZH_ALIASES = {
    '设计规范': ('docs/design-system.md', 'bare'),
    '规范': ('docs/design-system.md', 'bare'),
    '协议': ('docs/architecture/protocol.md', 'bare'),
    '产品': ('docs/PRODUCT.md', 'anchored'),
    '开发': ('docs/DEVELOPMENT.md', 'anchored'),
    '决策': ('docs/DECISIONS.md', 'anchored'),
}
# A 轨的 §N.N 若紧跟在这些扩展名之后（tokens.css §13 / app.js §x），指的是
# 原型资产或代码文件自己的编号系统，不是 docs 的节号，按测试标准明示除外。
EXTERNAL_SUFFIX = ('.css', '.js', '.ts', '.rs', '.json', '.sql')


def resolve(tok):
    """把一个文档标识（裸名 / 别名 / 路径）解析成仓库内路径；解析不出返回 None。

    裸名（不含 `/`）先查别名表；带路径的标识按存在性解析。
    """
    t = tok.strip('`').rstrip('.,;:、)）')
    if '/' not in t:
        base = os.path.basename(t)
        if base.endswith('.md'):
            base = base[:-3]
        # 中文别名的「锚定写法」带行号（`产品:92`）：先按别名查，再按去掉行号查。
        for cand_base in (base, base.rsplit(':', 1)[0] if base.rsplit(':', 1)[-1].isdigit() else base):
            if cand_base in ALIASES and os.path.exists(ALIASES[cand_base]):
                return ALIASES[cand_base]
            if cand_base in ZH_ALIASES:
                return ZH_ALIASES[cand_base][0]
    for cand in (t, 'docs/' + t, 'docs/architecture/' + t, '.mdflow/milestones/' + t):
        if os.path.exists(cand):
            return cand
    base = os.path.basename(t)
    if base.endswith('.md'):
        base = base[:-3]
    for alias, path in ALIASES.items():
        if alias.startswith(base) and os.path.exists(path):
            return path
    return None


_HEAD_CACHE = {}


def headings(path):
    """{节号: 行号}。节号取自标题行首的数字编号（'### 5.2 路由与深链接' → 5.2）。"""
    if path in _HEAD_CACHE:
        return _HEAD_CACHE[path]
    out = {}
    try:
        lines = open(path, encoding='utf-8').read().split('\n')
    except OSError:
        _HEAD_CACHE[path] = out
        return out
    for i, line in enumerate(lines, 1):
        m = re.match(r'^#{1,6}\s+(\d+(?:\.\d+)*)[.\s、]', line)
        if m:
            out.setdefault(m.group(1), i + 1)
    _HEAD_CACHE[path] = out
    return out


# 带 .md 的文档标识（允许路径前缀与前导 .）：`docs/x.md` / `x.md` / `.mdflow/…/x.md`
MD_PATH = r'\.?[A-Za-z0-9_][A-Za-z0-9_\-/\.]*\.md'
# 裸别名（不含 .md）：由上面两张表**派生**，不另抄一份。
# 抄一份的后果已经发生过：ZH_ALIASES 里的「产品/开发/决策」当初没抄进
# BARE_ALIAS，扫描根本匹配不到它们，resolve() 的中文分支成了死代码。
_BARE = sorted(set(ALIASES) | {k for k, (_, m) in ZH_ALIASES.items() if m == 'bare'},
               key=len, reverse=True)
BARE_ALIAS = '(?:' + '|'.join(re.escape(k) for k in _BARE) + ')'
# 必须带「:行号」的中文别名
_ZH_ANCHORED = sorted((k for k, (_, m) in ZH_ALIASES.items() if m == 'anchored'),
                       key=len, reverse=True)
ZH_ANCHORED = '(?:' + '|'.join(re.escape(k) for k in _ZH_ANCHORED) + r')\:\d+'
MENTION = re.compile(r'`?((?:' + MD_PATH + r')|(?:' + BARE_ALIAS + r')):?(?:\d+(?:-\d+)?)?`?'
                    r'|`?(' + ZH_ANCHORED + r')`?')
SECTION = re.compile(r'§\s*(\d+(?:\.\d+)*)')
# 代码锚点与文档锚点同规则（`:N` 必须落在稳定行上）。分开两个组是因为
# 两者的解析方式不同：`.md` 走上面的 resolve()（别名表 / docs/ 前缀），
# 代码路径要按仓库真实布局找（README 里常写 `routes/stream.rs` 这种
# 省略了 crate 前缀的简写）。
CODE_PATH = r'\.?[A-Za-z0-9_][A-Za-z0-9_\-/\.]*\.(?:rs|vue|ts|js|css|json|sql|sh)'
LINEANCHOR = re.compile(r'`?(' + MD_PATH + r'):(\d+)`?')
CODEANCHOR = re.compile(r'`?(' + CODE_PATH + r'):(\d+)`?')
# 「原 `x.md:N`」= 指向历史版本的行号（文件已改过），只查文件存在。
# 「原文锚点」/「历史锚点」是本仓库在缺陷池里声明同一件事的写法。
HISTORICAL = re.compile(r'(原\s*[`（(]?[^`）)]*\.md:\d+|原文锚点|历史锚点|原锚点)')
# 稳定锚点行：标题，或含反引号代码标识符的行。裸行号只有落在这种行上，
# 插一行才不会指错——指向散文行的行号是「迟早指错」的引用。
BACKTICK = re.compile(r'`[^`]+`')
HEADING_LINE = re.compile(r'^#{1,6}\s')

LIST_ITEM = re.compile(r'^\s*([-*+]|\d+[.)])\s')


def block_span(lines, i):
    """§ 锚点的上下文块：表格行 = 该行；列表项 = 该项（含续行）；否则 = 段落。"""
    line = lines[i]
    if line.lstrip().startswith('|'):
        return i, i
    if LIST_ITEM.match(line):
        j = i - 1
        while j >= 0 and lines[j].strip() and not LIST_ITEM.match(lines[j]) \
                and not lines[j].lstrip().startswith('|'):
            j -= 1
        start = j + 1
        j = i + 1
        while j < len(lines) and lines[j].strip() and not LIST_ITEM.match(lines[j]) \
                and not lines[j].lstrip().startswith('|'):
            j += 1
        return start, j - 1
    j = i - 1
    while j >= 0 and lines[j].strip():
        j -= 1
    start = j + 1
    j = i + 1
    while j < len(lines) and lines[j].strip():
        j += 1
    return start, j - 1


def collect_mentions(lines):
    """全文的文件提及，按 (行号, 列号) 排序。"""
    out = []
    for i, line in enumerate(lines, 1):
        for m in MENTION.finditer(line):
            tok = m.group(1) if m.group(1) is not None else m.group(2)
            rp = resolve(tok)
            if rp:
                out.append((i, m.start(), rp, tok))
    return out


bad_a, bad_b, pending = [], [], []
n_a = n_b = n_hist = n_code = 0


def check_a(path, lines, all_mentions):
    """A 轨：§N.N 必须能解析到某个候选归属文件的小节标题。

    归属口径 = **该 § 之前最近出现的显式文件名**（跨行也算，规格原意如此）；
    归属文件里没有该小节时，再退一步看同一上下文块里的其它提及、上一处提及
    与本文档自身——规格文本里的「§8.1」有时指的是设计规范而不是紧邻的协议文档。
    """
    global n_a
    for lineno, line in enumerate(lines, 1):
        bs, be = block_span(lines, lineno - 1)
        block = [m for m in all_mentions if bs + 1 <= m[0] <= be + 1]
        for m in SECTION.finditer(line):
            if line[:m.start()].rstrip().endswith(EXTERNAL_SUFFIX):
                continue
            sec = m.group(1)
            before = [x for x in block if (x[0], x[1]) < (lineno, m.start())]
            after = [x for x in block if (x[0], x[1]) > (lineno, m.start())]
            earlier = [x for x in all_mentions if (x[0], x[1]) < (lineno, m.start())]
            cands = []
            for group in (before, after, earlier[-1:] if earlier else []):
                for x in group:
                    if x[2] not in cands:
                        cands.append(x[2])
            if path not in cands:
                cands.append(path)
            if not cands:
                continue
            n_a += 1
            if not any(sec in headings(c) for c in cands):
                bad_a.append((path, lineno, cands[0], sec))


def resolve_code(tok):
    """把代码文件锚点解析成仓库内真实路径；找不到返回 None。

    文档里写代码路径常用**省略 crate 前缀**的简写（`routes/stream.rs`、
    `SessionPage.vue`），那是给人读的短名，不是 os.path 能直接命中的路径。
    所以这里按已知布局逐级试，而不是只查一次 exists：
    先按原样（已是全路径的情形），再试 crates/*/src/ 与 packages/web/src/，
    最后按 basename 在全仓搜（同名文件多处命中时取第一个并让调用方报歧义）。

    `packages/web/src` 底下**没有层级假设**：v0.7.0 起前端按领域优先重组，
    业务代码在 `domains/<领域>/`，共享组件在 `domains/shared/components/`，
    纯函数工具在 `shared/`。这里逐条列 glob 而不是写成 `**`，是因为 `**`
    会把 `SessionPage.vue` 同时匹配到 `domains/session/` 与任何同名副本，
    而**歧义必须被报出来**——递归通配让「同名两处」变得安静。
    """
    t = tok.strip('`').rstrip('.,;:、)）')
    if os.path.exists(t):
        return t
    for pat in ('crates/*/src/' + t, 'packages/web/src/' + t,
                # 前端按**领域优先**重组后的三层布局（v0.7.0 子任务 1.A）：
                # `pages/` 只剩登录 / setup / 设置这三个「外壳级」页面，业务页面
                # 与它们的同目录组件、api 都在 `domains/<领域>/` 下，跨领域共享的
                # 组件在 `domains/shared/components/`，纯函数工具在 `shared/`。
                # 这三条 glob 是在 `pages/` 与 `components/` 两条之外**追加**的，
                # 不是替换：`pages/` 里仍有三个真文件，删掉那条会让
                # `pages/SettingsPage.vue:N` 这类简写锚点解析失败。
                'packages/web/src/domains/*/' + t,
                'packages/web/src/domains/*/components/' + t,
                'packages/web/src/shared/' + t,
                'packages/web/src/pages/' + t, 'packages/web/src/components/' + t,
                'crates/*/' + t):
        hits = sorted(glob.glob(pat))
        if len(hits) == 1:
            return hits[0]
        if len(hits) > 1:
            return 'AMBIGUOUS:' + '|'.join(hits)
    base = os.path.basename(t)
    hits = [q for q in CODE_INDEX.get(base, []) if q.endswith(t) or q.endswith('/' + t)]
    if len(hits) == 1:
        return hits[0]
    if len(hits) > 1:
        return 'AMBIGUOUS:' + '|'.join(hits)
    return None


_CODE_INDEX = None


def code_index():
    """{basename: [全路径, ...]}：全仓代码文件按文件名索引（不含 target/）。"""
    global _CODE_INDEX
    if _CODE_INDEX is None:
        idx = {}
        for root, dirs, names in os.walk('.'):
            dirs[:] = [d for d in dirs if d not in ('target', 'node_modules', '.git')]
            for name in names:
                if name.endswith(('.rs', '.vue', '.ts', '.js', '.css', '.json', '.sql', '.sh')):
                    idx.setdefault(name, []).append(os.path.join(root, name)[2:])
        _CODE_INDEX = idx
    return _CODE_INDEX


CODE_INDEX = code_index()


def check_b(path, lines, own):
    """B 轨：`xxx.md:N` 裸行号必须落在目标文件的稳定锚点行上。

    只查「行号在范围内且非空」拦不住表格插行：插一行后行号仍然合法，
    指向的却是另一行。所以额外要求目标是**稳定锚点行**（标题 / 含代码标识符的行）。

    **仓库自有文档不满足即判红**；里程碑文档里的裸行号锚点只报不红
    （`--strict-anchors` 才判红）——那批锚点归主线迁移（见 M1 里程碑 Bugs 表
    910 条），门禁在这里只负责「把漂移摆出来」，不负责替主线改文档。
    """
    global n_b, n_hist, n_code
    for lineno, line in enumerate(lines, 1):
        historical = bool(HISTORICAL.search(line))
        for m in LINEANCHOR.finditer(line):
            rp = resolve(m.group(1))
            if rp is None:
                item = (path, lineno, m.group(1), int(m.group(2)), 'FILE MISSING')
                (bad_b if own else pending).append(item)
                continue
            if historical:
                n_hist += 1
                continue
            item = verify_line_anchor(path, lineno, m.group(1), rp, int(m.group(2)))
            if item is None:
                n_b += 1
            else:
                (bad_b if own else pending).append(item)
        # 代码锚点（`.rs` / `.vue` / …）。原来只有 `.md` 一类，于是
        # `scripts/e2e-m1/README.md` 里 17 处代码行号引用**一个都没被查过**：
        # 它们全指错了也没人发现。同一套规则（解析 → 范围 → 稳定行）搬过来即可，
        # 判红口径也一致：仓库自有文档判红，里程碑文档只报。
        for m in CODEANCHOR.finditer(line):
            rp = resolve_code(m.group(1))
            if rp is None:
                item = (path, lineno, m.group(1), int(m.group(2)), 'CODE FILE MISSING')
                (bad_b if own else pending).append(item)
                continue
            if rp.startswith('AMBIGUOUS:'):
                cands = rp[len('AMBIGUOUS:'):].split('|')
                item = (path, lineno, m.group(1), int(m.group(2)),
                        'CODE FILE AMBIGUOUS (%d candidates: %s)' % (len(cands), ', '.join(cands[:3])))
                (bad_b if own else pending).append(item)
                continue
            if historical:
                n_hist += 1
                continue
            item = verify_code_anchor(path, lineno, m.group(1), rp, int(m.group(2)))
            if item is None:
                n_code += 1
            else:
                (bad_b if own else pending).append(item)


def verify_code_anchor(src, lineno, tok, target, n):
    """代码锚点的校验：文件能解析 → 行号在范围内且非空。

    **不要求「稳定锚点行」**（md 那套「标题 / 含反引号」的判据）——那是对付
    文档插行的：md 里插一行表格，行号仍然合法但指向了另一行，所以要求目标
    落在不会被插行穿过的地方。代码文件没有这个结构：`.rs` 的行就是语句，
    没有「标题行」这个概念，而绝大多数语句也不含反引号。把 md 的判据搬过来
    等于要求每条代码锚点都指向注释或字符串——那只会把锚点逼到注释上。

    代价是代码行号确实会随编辑漂移，这一点由 `check.sh` 每次实跑来兜：
    行号越界 / 空行立刻报，指向了别处则要等下一轮审查发现。
    """
    fl = open(target, encoding='utf-8').read().split('\n')
    if n < 1 or n > len(fl) or not fl[n - 1].strip():
        return (src, lineno, tok, n, 'LINE EMPTY OR OUT OF RANGE')
    return None


def verify_line_anchor(src, lineno, tok, target, n):
    """裸行号锚点的三段校验：能解析 → 行号在范围内且非空 → 落在稳定行上。

    只查「行号在范围内且非空」拦不住表格插行：插一行后行号仍然合法，
    指向的却是另一行。所以额外要求目标是**稳定锚点行**（标题 / 含代码标识符的行）。

    返回 None 表示通过；否则返回一条待登记的五元组。
    """
    fl = open(target, encoding='utf-8').read().split('\n')
    if n < 1 or n > len(fl) or not fl[n - 1].strip():
        return (src, lineno, tok, n, 'LINE EMPTY OR OUT OF RANGE')
    if not (HEADING_LINE.match(fl[n - 1]) or BACKTICK.search(fl[n - 1])):
        return (src, lineno, tok, n, 'NOT A STABLE ANCHOR LINE')
    return None


for path in MILESTONE + OWN:
    lines = open(path, encoding='utf-8').read().split('\n')
    mentions = collect_mentions(lines)
    check_a(path, lines, mentions)
    check_b(path, lines, own=path in OWN)

# --strict-anchors：把「待主线迁移的裸行号锚点」也算失败（主线迁移完就该关掉这个口子）
if STRICT:
    bad_b.extend(pending)
    pending = []

print(f"  A 轨（§N.N 稳定锚）：{n_a} 处待解析，{'全部解析到小节标题' if not bad_a else str(len(bad_a)) + ' 处悬空'}")
print(f"  B 轨（file.md:N 裸行号）：{n_b + n_hist + len(pending)} 处（含 {n_hist} 处历史锚点仅查文件存在），"
      f"{'无失效' if not bad_b else str(len(bad_b)) + ' 处失效'}")
print(f"  B 轨代码锚点（.rs/.vue/…:N）：{n_code} 处，"
      f"{'全部有效' if not bad_b else str(len(bad_b)) + ' 处失效'}")
if pending:
    print(f"  ⚠️  里程碑文档待迁移的裸行号锚点：{len(pending)} 处（指向非稳定锚点行 / 行号已越界，"
          f"插一行即指错；迁移清单见 M1 里程碑 Bugs 表 910 条）")
    for p, ln, f, n, why in pending:
        print(f"     · {p}:{ln} {f}:{n} {why}")
if bad_a:
    for p, ln, owner, sec in bad_a:
        print(f"  ❌ {p}:{ln} {owner} §{sec} NOT A SECTION HEADING")
if bad_b:
    for p, ln, f, n, why in bad_b:
        print(f"  ❌ {p}:{ln} {f}:{n} {why}")
if bad_a or bad_b:
    sys.exit(1)
PYEOF
  [[ $? -ne 0 ]] && fail=1

# C4「生产代码不得用 unwrap_or_default」已删除：那条规则把「默认值有语义」的
# 正常写法（缺字段 = 空串/空列表）与「静默吞错」判成同一件事，于是每次都要写
# 一段与 `unwrap_or_default` 等价的显式分支才能过闸。现在只禁静默吞错，判据见
# `AGENTS.md`「生产代码禁 panic 铁律」里 `unwrap_or_default` 那一段。

echo "== C4 源文件不得含 NUL 字节（会让 grep 类门禁在该文件上静默失明）=="
# 为什么需要这一条：Rust 编译器**接受**源文件里的字面 NUL（它是合法 UTF-8），
# 构建不会红；但 grep 会把整个文件判定为 binary，从此这个文件对 C2/C3 以及
# 所有 grep 门禁**静默失明**。真实案例：`connector/git.rs` 的注释里为了描述
# 「一个字节」写了字面 NUL，注释本身没问题，门禁却失效了。
hits=$(python3 - <<'PYEOF'
import glob
bad = []
for pat in ('crates/**/*.rs', 'packages/web/src/**/*.ts',
            'packages/web/src/**/*.vue', 'scripts/**/*.sh'):
    for f in glob.glob(pat, recursive=True):
        try:
            d = open(f, 'rb').read()
        except OSError:
            continue
        if b'\x00' in d:
            ln = d[:d.index(b'\x00')].count(b'\n') + 1
            bad.append(f'{f}:{ln}')
print('\n'.join(bad))
PYEOF
)
if [[ -z "$hits" ]]; then ok "无 NUL 字节"; else bad "源文件含 NUL 字节（改用转义写法 \\0）："; echo "$hits" | sed 's/^/      /'; fi

echo "== C5 设计 token / 组件样式的真相源存在且唯一（真身 = packages/web/src/styles/）=="
# 真相源只有一份：`packages/web/src/styles/{tokens,components}.css`。
#
# 判定标准是「**有没有定义 `--af-*` 变量**」，不是文件名。理由：原型
# （`prototype/assets/`）允许放同名转发 shim —— 只 `@import` 生产副本，
# 自己一行变量都不定义。这种副本不会造成「两份各说各话」，反而是让原型
# 与实现共用一套 token 的正确做法。按文件名判定会把合法 shim 误杀。
#
# 名字仍叫 C5，不重编号（与 AGENTS.md 约束编号同理：引用处太多，顶替即制造谎言）。
style_hits=""
for name in tokens.css components.css; do
  prod="packages/web/src/styles/$name"
  if [ ! -f "$prod" ]; then
    style_hits="$style_hits 缺失 $prod\n"
  fi
  # 任何同名文件：只要它自己定义了 --af-* 变量，就是第二份真相源
  while IFS= read -r dup; do
    [ -z "$dup" ] && continue
    if grep -qE '^\s*--af-[a-z0-9-]+\s*:' "$dup" 2>/dev/null; then
      style_hits="$style_hits $dup 定义了 --af-* 变量（真相源只能是 $prod）\n"
    fi
  done < <(find . -name "$name" -not -path "./$prod" -not -path "./node_modules/*" -not -path "./target/*" 2>/dev/null)
done
if [[ -z "$style_hits" ]]; then
  ok "设计 token / 组件样式只有一份真相源"
else
  bad "设计 token / 组件样式真相源有问题："
  printf '%b' "$style_hits" | sed 's/^/      /'
fi

echo "== C6 systemd unit 必须与 afcli 的真实契约一致 =="
# `deploy/afcli.service` 是唯一一份「怎么把 afcli 托给 systemd」的说明，而它
# 描述的四件事（前台不 daemon 化 / 两个环境变量名 / supervisor 管 worker、systemd
# 管进程）都在代码里有真相源。unit 与代码脱节时不会有任何编译错误——只会在
# 一台真设备上表现为「服务卡在 activating」或「连到本机 Hub 永远注册不上」。
python3 - <<'PYEOF'
import re, sys, io, os

UNIT = 'deploy/afcli.service'
ENVX = 'deploy/afcli.env.example'
bad = []

def read(p):
    if not os.path.exists(p):
        bad.append(f'缺少 {p}')
        return ''
    return io.open(p, encoding='utf-8').read()

unit = read(UNIT)
read(ENVX)

if unit:
    # ① afcli 不会 daemon 化 → `Type=` 必须是 simple。
    # 真相源：`crates/node-cli/src/supervisor.rs` 的 supervise() 是前台无限 loop。
    m = re.search(r'^Type=(\S+)', unit, re.M)
    if not m:
        bad.append('unit 没有写 Type=')
    elif m.group(1) != 'simple':
        bad.append(f'Type={m.group(1)}：afcli 不会 daemon 化，必须是 Type=simple')

    # ② 两个环境变量名必须与代码一致。改名而 unit 不改的症状是「配了不生效」。
    src = io.open('crates/node-cli/src/runargs.rs', encoding='utf-8').read()
    for name in ('AFCLI_HUB_URL', 'AFCLI_DATA_DIR'):
        if name in src and name not in unit and name not in io.open(ENVX, encoding='utf-8').read():
            bad.append(f'{name} 既不在 unit 也不在 env 样例里，但代码在读它')

    # ③ 崩溃自拉起。supervisor 管 worker 生命周期，systemd 管进程存活——
    #    少了这一层，supervisor 自己被 OOM kill 之后没人把它拉起来。
    if not re.search(r'^Restart=(always|on-failure)', unit, re.M):
        bad.append('unit 没有 Restart=：supervisor 死了没人拉，整台设备就掉了')

    # ④ 不得以 root 跑：afcli 持有设备私钥。
    if 'User=root' in unit:
        bad.append('User=root：设备私钥不该交出去')

    # ⑤ 禁止在 unit 里做 daemon 化包装（DECISIONS.md D1：supervisor 管生命周期）。
    for pat in ('nohup', 'setsid', '&'):
        if re.search(r'^ExecStart=.*' + re.escape(pat), unit, re.M):
            bad.append(f'ExecStart 里有 {pat}：会把 supervisor 的退出观测全废掉')

for b in bad:
    print(f'      {b}')
sys.exit(1 if bad else 0)
PYEOF
  [[ $? -ne 0 ]] && { echo "  ❌ systemd unit 与代码契约不一致"; fail=1; } || ok "unit 与代码契约一致"

echo "== C7 release.yml 的 gate 必须跑齐 mise.toml 的全部门禁，且产线各 job 都等它 =="
# 唯一权威是 `mise.toml` 的 `[tasks.ci].depends`——本地 `mise run ci` 跑的就是它。
# `release.yml` 的 `gate` job 是产线里的同一串命令，**必须逐条覆盖**：
# 少抄一条，产线就少跑一道闸而没人发现（`ci.yml` 曾经是第二份拷贝，靠它比对；
# 现在那份已删除，改成直接与 `mise.toml` 比 —— 只剩一个真相源，也就不存在
# 「两份清单互相漂移」这件事了）。
python3 - <<'PYEOF'
import sys, os, re
try:
    import yaml
except ImportError:
    print('      缺 PyYAML：无法核对 release.yml（不要跳过——跳过等于假绿）')
    sys.exit(1)

bad = []

RELEASE_YML = '.github/workflows/release.yml'
MISE_TOML = 'mise.toml'

def read_mise_ci_tasks():
    """从 `mise.toml` 的 `[tasks.ci]` 读 `depends` 列表。

    手写解析而不是引 tomllib：本仓要兼容 Python 3.9（`tomllib` 是 3.11+），
    而这一段的输入形状固定（`depends = ["a", "b"]` 单行数组）。
    读不到就返回 None，由调用方判成 bad —— 静默返回空列表会让
    「不知道有哪些门禁」冒充「没有门禁」。
    """
    try:
        with open(MISE_TOML, encoding='utf-8') as f:
            text = f.read()
    except OSError as e:
        bad.append(f'{MISE_TOML} 读取失败：{e}')
        return None
    m = re.search(r'^\[tasks\.ci\][^\[]*?^depends\s*=\s*\[(.*?)\]', text, re.M | re.S)
    if not m:
        bad.append(f'{MISE_TOML} 里找不到 [tasks.ci] 的 depends')
        return None
    tasks = re.findall(r'"([^"]+)"', m.group(1))
    if not tasks:
        bad.append(f'{MISE_TOML} 的 [tasks.ci].depends 解析出 0 项')
        return None
    return tasks

_parsed = {}
def load_jobs(path):
    if path not in _parsed:
        try:
            with open(path, encoding='utf-8') as f:
                _parsed[path] = (yaml.safe_load(f) or {}).get('jobs') or {}
        except yaml.YAMLError as e:
            # 解析失败要说清「是文件本身写坏了」，不能让它冒成 traceback 被外层
            # 一句笼统的失败概括掉——那会把排障方向引到「去比对门禁步」，
            # 而真正原因是语法错。
            bad.append(f'{path} YAML 解析失败：{e}')
            _parsed[path] = {}
        except OSError as e:
            bad.append(f'{path} 读取失败：{e}')
            _parsed[path] = {}
    return _parsed[path]

def gate_tasks(path, job_names):
    """取一组 job 里「`mise run <task>）` 结尾」的那些步，返回 {task: (job, run)}。

    门禁拆成 4 个并行 job 后（v0.7.1 收尾的提速改造），单 job 内的步骤不再
    覆盖全部清单，必须扫**合集**。task 出现在多个 job 里不算错也不算重复——
    例如未来某 job 既 lint 又 test——但同一 task 的 run 文本若不一致，
    说明有人复制步骤后改了一份忘另一份，必须红。
    """
    jobs = load_jobs(path)
    out = {}
    for job in job_names:
        if job not in jobs:
            bad.append(f'{path} 里没有 job `{job}`')
            continue
        for s in jobs[job].get('steps') or []:
            name = s.get('name') or ''
            m = re.search(r'mise run ([a-z-]+)）', name)
            if m:
                task = m.group(1)
                run = (s.get('run') or '').strip()
                if task in out and out[task][1] != run:
                    bad.append(f'`{task}` 在 `{out[task][0]}` 与 `{job}` 里都出现，'
                               f'但两处的 run 不一致（复制步骤后只改了一份？）')
                out[task] = (job, run)
    return out

want = read_mise_ci_tasks()
GATE_JOBS = ['gate-static', 'gate-test', 'gate-coverage', 'gate-web']
got = gate_tasks(RELEASE_YML, GATE_JOBS)

if want is not None:
    if not got:
        bad.append(f'{RELEASE_YML} 的 {GATE_JOBS} 里没有一条 `mise run` 门禁步')
    for t in sorted(set(want) - set(got)):
        bad.append(f'`{t}` 在 mise.toml 的 [tasks.ci] 里，但 {RELEASE_YML} 的 gate job 们没跑')
    for t in sorted(set(got) - set(want)):
        bad.append(f'`{t}` 在 {RELEASE_YML} 的 gate job 们里，但 mise.toml 的 [tasks.ci] 没有')
    if want is not None and got and not (set(want) ^ set(got)):
        where = ', '.join(f'{t}→{got[t][0]}' for t in sorted(got))
        print(f'      产线 gate（{len(GATE_JOBS)} 个 job 合集）覆盖 mise.toml 的全部 {len(want)} 道门禁')
        print(f'      {where}')

# 产线各 job 必须等全部门禁：门禁红时不许推镜像/发 Release。
# 这一段没有 `if os.path.exists(...)` 守卫：文件缺失/读不了时 `load_jobs` 已经把
# 原因写进 `bad`，而门禁失败的正确反应就是「红」，不是「跳过这段」。用守卫包起来
# 会让「文件找不到」退化成「产线各 job 都等 gate ✓」——断言恰恰在最该红时消失。
jobs = load_jobs(RELEASE_YML)
if not jobs:
    bad.append(f'{RELEASE_YML} 没有可解析的 job：产线各 job 是否等 gate 无从判断')
else:
    missing_gates = [g for g in GATE_JOBS if g not in jobs]
    if missing_gates:
        bad.append(f'{RELEASE_YML} 缺少 gate job：{", ".join(missing_gates)}'
                   '——门禁清单被拆分后，少一个 job 就少跑一道闸')
    for j in ('build-frontend', 'build-binaries', 'docker-hub', 'docker-node', 'release'):
        if j not in jobs:
            bad.append(f'{RELEASE_YML} 里没有 job `{j}`：无法确认它等 gate')
            continue
        needs = jobs[j].get('needs') or []
        if isinstance(needs, str):
            needs = [needs]
        for g in GATE_JOBS:
            if g not in needs:
                bad.append(f'{j} 的 needs 里没有 {g}：这一道门禁红时它照样会跑')

for b in bad:
    print(f'      {b}')
sys.exit(1 if bad else 0)
PYEOF
  [[ $? -ne 0 ]] && { echo "  ❌ release.yml 的 gate 与 mise.toml 门禁清单不一致"; fail=1; } || ok "产线 gate 覆盖 mise.toml 全部门禁，且各 job 都等 gate"

echo "== C8 release.yml 的 gate 必须装齐门禁用到的 rust 组件 =="
# 这条门禁是**事后补的**，理由值得记住：`dtolnay/rust-toolchain` 带
# `--profile minimal`，只装**显式列出**的组件。首次真跑 CI 时 gate 只写了
# `llvm-tools-preview`，于是 `cargo fmt --check` 报
# `error: 'cargo-fmt' is not installed for the toolchain`，而**本地全绿**——
# 因为本地 `cargo fmt` 找不到自带的 `cargo-fmt` 时，会从 `$PATH` 里捞到
# rustup 的 shim，进而退回 `stable` 那个 rustfmt，静默用错工具链还能过。
# 「本地绿、CI 红」这类差异不会自己暴露，只能靠一条能读 workflow 的门禁钉住。
python3 - <<'PYEOF'
import sys, re
try:
    import yaml
except ImportError:
    print('      缺 PyYAML：无法核对 release.yml（不要跳过——跳过等于假绿）')
    sys.exit(1)

RELEASE_YML = '.github/workflows/release.yml'

# 门禁命令 → 它需要的 cargo 子命令所对应的 rust 组件。
# 只登记**确实是独立组件**的：`cargo build` / `cargo test` / `cargo check`
# 只需 rustc + rust-std（minimal profile 自带），不必列进来——多登记会让
# 「装齐」变成「装一堆用不上的」，那不是准确性。
#
# **`cargo llvm-cov` 不在这里**，而且这是个教训：v0.7.2 之前它由
# `scripts/check-coverage.sh` 内部调用，不出现在任何 workflow 的 `run:` 文本里，
# 靠扫 `run:` 永远扫不到——C8 据此曾对 `llvm-tools-preview` 长期假绿
# （删掉该组件也报红不了），直到拆分 gate 做变异验证才暴露。它的等价判据
# 是下面的 `install-action` 配对：装 cargo-llvm-cov 的 job 必须同时声明
# llvm-tools-preview 组件（脚本用「llvm-cov 可执行文件 + 工具链自带
# llvm-tools」两者，缺任一都跑不起来）。
# v0.7.2 起覆盖率步骤把 `cargo llvm-cov` 直接写在 `run:` 里（不再经脚本包一层），
# 所以 1c) 的反向配对改成按**命令文本**断言，见那里。
COMPONENT_OF = {
    'cargo fmt': 'rustfmt',
    'cargo clippy': 'clippy',
}
# `taiki-e/install-action@cargo-llvm-cov`（安装步）→ 必须同时有的组件。
REQUIRES_COMPONENT_WITH_USES = {
    'taiki-e/install-action@cargo-llvm-cov': 'llvm-tools-preview',
}

GATE_JOBS = ['gate-static', 'gate-test', 'gate-coverage', 'gate-web']

bad = []
try:
    with open(RELEASE_YML, encoding='utf-8') as f:
        jobs = (yaml.safe_load(f) or {}).get('jobs') or {}
except yaml.YAMLError as e:
    bad.append(f'{RELEASE_YML} YAML 解析失败：{e}')
    jobs = {}
except OSError as e:
    bad.append(f'{RELEASE_YML} 读取失败：{e}')
    jobs = {}

# 门禁拆成 4 个 job 后，「装了哪些组件」与「跑了哪些命令」都必须看**合集**：
# 命令在 gate-test 里跑、组件在 gate-coverage 里装，是合法的分工（每个 job 只装
# 自己要的）；反过来命令所在的 job 没装它需要的组件才是错。
steps = []
for jn in GATE_JOBS:
    if jn not in jobs:
        # 缺 job 的错误由 C7 报（那里带任务清单语义），这里不重复报。
        continue
    steps.extend(jobs[jn].get('steps') or [])
if not steps:
    bad.append(f'{RELEASE_YML} 里一个 gate job 都没有：无法核对组件（先看 C7 报了什么）')
else:
    # 1) 合集里实际跑到的命令 → 需要的组件
    needed = {}
    for s in steps:
        run = s.get('run') or ''
        for cmd, comp in COMPONENT_OF.items():
            if re.search(r'\b' + re.escape(cmd) + r'\b', run):
                needed[comp] = cmd

    # 1b) 安装步配对：某 job 装了 X 工具，就必须同时声明 X 依赖的组件。
    # `run:` 文本扫描对「脚本内部调用的命令」是瞎的（llvm-cov 的教训，
    # 见 COMPONENT_OF 上面的注释），所以对有专用安装步的工具改用 uses 断言。
    for s in steps:
        uses = str(s.get('uses') or '')
        for action, comp in REQUIRES_COMPONENT_WITH_USES.items():
            if uses.startswith(action):
                needed[comp] = f'{action}（{uses}）'

    # 2) 合集里**声明**装的组件（可能出现在多个 job，合并）
    declared = set()
    for s in steps:
        with_ = s.get('with') or {}
        for c in str(with_.get('components') or '').split(','):
            c = c.strip()
            if c:
                declared.add(c)

    # 1c) 反向配对：反向断言的缺口（变异4 教训）。删掉 装 cargo-llvm-cov 安装步后，
    # needed 里 llvm-tools 消失、断言空转 → 假绿。v0.7.2 起覆盖率门禁是直接写
    # `cargo llvm-cov …` 的步骤（不再经 `check-coverage.sh` 包一层），所以按
    # **命令文本**断言：步骤里出现 `cargo llvm-cov` 就必须有对应的安装步。
    # 按文本而不是按脚本文件名，才不会因为「换了实现载体」而静默失效
    # ——旧写法只认 `check-coverage.sh`，那脚本删掉后这条断言会**恒假通过**。
    has_llvm_cov_step = any(
        str(s.get('uses') or '').startswith('taiki-e/install-action@cargo-llvm-cov')
        for s in steps
    )
    has_cov_gate = any('cargo llvm-cov' in (s.get('run') or '') for s in steps)
    if has_cov_gate and not has_llvm_cov_step:
        bad.append('gate job 们跑了覆盖率门禁（`cargo llvm-cov`），'
                   '但没有 装 cargo-llvm-cov 的安装步（taiki-e/install-action@cargo-llvm-cov）'
                   '——工具不存在时门禁 FATAL')

    if not needed:
        # 一个组件都不需要，说明门禁清单已经面目全非——不当成通过。
        bad.append('gate job 们里读不到任何需要 rust 组件的门禁（`cargo fmt` / '
                   '`cargo clippy` / cargo-llvm-cov 安装步）：不是「无需组件」，'
                   '更可能是步骤被改名或挪走了，先确认门禁还在跑')
    for comp in sorted(set(needed) - declared):
        bad.append(f'`{needed[comp]}` 需要组件 `{comp}`，但 gate job 们的 '
                   f'`components:` 里没有（CI 上会报 '
                   f'"\'{comp}\' is not installed for the toolchain"）')

    if needed and not (set(needed) - declared) and not bad:
        print(f'      gate job 们已装齐门禁用到的全部组件：{", ".join(sorted(needed))}')

for b in bad:
    print(f'      {b}')
sys.exit(1 if bad else 0)
PYEOF
  [[ $? -ne 0 ]] && { echo "  ❌ release.yml 的 gate 未装齐门禁用到的 rust 组件"; fail=1; } || ok "gate 装齐门禁用到的 rust 组件"

echo
echo "== C9 workflow 里 gh CLI 的 --json 字段必须真实存在 =="
# 这条门禁是**事后补的**，理由值得记住：`gh release view --json isLatest`
# 里的 `isLatest` **根本不是 gh CLI 的合法字段**（字段表只有 `isPrerelease` /
# `isDraft` / `tagName` / `assets` / `publishedAt` 等），带上它 gh 直接报
# `Unknown JSON field: "isLatest"` 并以非零码退出——在 `set -euo pipefail`
# 下整步红。v0.7.1 的 release 步首次真跑到就是这么红的（CI run 42043667668）：
# 前面的门禁先红，release 步一直没被真执行过，这个缺陷藏了很久。
#
# 为什么值得单列一条门禁：这类缺陷**没有任何本地检查能发现**——
# 它不违反 YAML 语法、不违反 shell 语法，只在「真跑 CI 且真到那一步」时
# 才显形。而 release 步在门禁之后，一旦前面的门禁红，release 步就永远
# 得不到执行——缺陷与它的遮蔽条件是同一个条件。
#
# 判据办法：`gh <子命令> --json` 不带值时会列出该子命令的全部合法字段，
# 于是「workflow 里用到的字段是否都在表里」可以静态比对。
# 字段表随 gh 版本变化，所以**只查「用到的字段 ⊆ 该 gh 的字段表」**，
# 不反向要求「字段表里的都要用」。
python3 - <<'PYEOF'
import re, subprocess, sys

RELEASE_YML = '.github/workflows/release.yml'
bad = []

try:
    with open(RELEASE_YML, encoding='utf-8') as f:
        yml = f.read()
except OSError as e:
    print(f'      读取 {RELEASE_YML} 失败：{e}')
    sys.exit(1)

# 抓出所有 `gh <子命令...> --json <字段列表>` 的用法。
#
# **子命令可能是多词**（`gh release view --json`），且**字段表是按完整子命令
# 路径查的**——`gh release --json` 不是合法用法（它会打印子命令列表），
# 按「第一个词」去查会拿到一份错的字段表（把 `create` / `delete` 这些
# 子命令名当成字段名，于是合法字段 `isPrerelease` 被误判为不存在）。
#
# **`gh` 与 `--json` 之间可以有任意参数**（`gh release view "$TAG" --json`），
# 所以中间不能只允许空白——用「非换行的任意内容」连接。
#
# 字段列表可能跨行（`\` 续行），所以先把续行合并再匹配。
flat = re.sub(r'\\\s*\n\s*', ' ', yml)
# 抓「gh」与「--json」之间的**全部**内容（可能是 `release view "$TAG"` 这种
# 「子命令 + 参数」的混合）。子命令路径由 gh 自己判定（见下），不在这里猜。
BROAD = re.compile(r'\bgh\s+(.+?)\s+--json\s+'
                   r'([A-Za-z][A-Za-z0-9]*(?:\s*,\s*[A-Za-z][A-Za-z0-9]*)*)')

# **必须剔掉注释行**：门禁的说明文字里会引用出错的写法当反例
# （本门禁自己就写着「不能用 `gh release view --json isLatest`」），
# 把注释也当用法就会自己把自己判红。shell 注释只有「整行以 `#` 开头」
# 和「`#` 之后到行尾」两种，但 `#` 出现在字符串里的情况极少，
# 误剔一行注释的代价远小于漏判。
def strip_comments(text):
    out = []
    for line in text.split('\n'):
        stripped = line.lstrip()
        if stripped.startswith('#'):
            continue
        out.append(line)
    return '\n'.join(out)


def field_table(words):
    """问 gh：这些词组成的子命令路径有哪些合法 --json 字段。

    返回 (fields, errkind)：
      fields 非空 → 成功，fields 是合法字段集合；
      errkind == 'no-such-command' → 路径不对（gh 打印的是子命令列表）；
      errkind == 'no-json' → 该子命令不支持 --json；
      errkind == 'error' → 其他问题（gh 异常）。

    **子命令路径由 gh 自己判定**：文本里 `gh` 与 `--json` 之间混着参数
    （`gh release view "$TAG" --json`），靠正则猜哪几个词是子命令名会错
    （曾把 `gh release view --json` 的路径截成 `release`，于是查到的是
    「子命令列表」而非字段表，把合法字段 `isPrerelease` 误判为不存在）。
    办法是**逐词回退**：先试全部词，gh 说「不认识的命令」就去掉最后一个词
    再试，直到拿到字段表或用完为止。
    """
    words = list(words)
    while words:
        try:
            out = subprocess.run(['gh'] + words + ['--json'],
                                 capture_output=True, text=True, timeout=60)
        except (OSError, subprocess.SubprocessError):
            return set(), 'error'
        err = out.stderr or ''
        # **字段表必须以「Specify one or more comma-separated fields」为锚点**。
        # 不加锚点会把「Available commands:」下面那些**子命令名**
        # （create / delete / view / upload…）当成字段名——于是
        # `gh release viewx --json`（子命令不存在）会拿到一份
        # {create, delete, view, ...} 的假字段表，把合法字段
        # `isPrerelease` 误判为不存在。锚点区分两种输出形态：
        #   · 字段表 → "Specify one or more comma-separated fields for `--json`:"
        #   · 子命令列表 → "Available commands:"
        if 'Specify one or more' in err:
            head, _, tail = err.partition('Specify one or more')
            fields = set(re.findall(r'^\s+([A-Za-z][A-Za-z0-9]*)\s*$', tail, re.M))
            if not fields:
                fields = set(re.findall(r'^\s*([A-Za-z][A-Za-z0-9]*)\s*$', tail, re.M))
            return (fields, 'ok') if fields else (set(), 'no-json')
        if 'Available commands:' in err or 'unknown command' in err:
            words.pop()
            continue
        return set(), 'error'
    return set(), 'no-such-command'


seen = {}  # (子命令路径, 字段) -> 用法样例
for m in BROAD.finditer(strip_comments(flat)):
    words = m.group(1).split()
    for fld in m.group(2).split(','):
        fld = fld.strip()
        if fld:
            seen.setdefault((fld, tuple(words)), m.group(0)[:80])

if not seen:
    print('      workflow 里没有 `gh ... --json` 的用法（若已改用 gh api，本门禁无事可做）')
    sys.exit(0)

for (fld, words), sample in sorted(seen.items()):
    fields, kind = field_table(words)
    if kind == 'ok':
        if fld not in fields:
            bad.append(f'`gh {" ".join(words)} --json {fld}` 用了**不存在的字段**'
                       f'（该 gh 版本的合法字段里没有它）——CI 上会报 '
                       f'`Unknown JSON field: "{fld}"` 并以非零码退出'
                       f'（用法：{sample}…）')
    elif kind == 'no-such-command':
        bad.append(f'`gh {" ".join(words)} --json` 用的是**不存在的子命令路径**'
                   f'——CI 上 gh 会报 `unknown command`（用法：{sample}…）')
    elif kind == 'no-json':
        bad.append(f'`gh {" ".join(words)} --json` 的子命令**不支持 --json**'
                   f'（用法：{sample}…）')
    else:
        print(f'      跑 `gh {" ".join(words)} --json` 失败：本门禁查不了，'
              f'不当作通过——跳过等于假绿')
        sys.exit(1)

for b in bad:
    print(f'      {b}')
sys.exit(1 if bad else 0)
PYEOF
  [[ $? -ne 0 ]] && { echo "  ❌ workflow 用了 gh CLI 不存在的 --json 字段"; fail=1; } || ok "workflow 的 gh --json 字段都真实存在"

echo
echo "== C10 release.yml 不得把前端 / Rust 编译重复铺开 =="
# 前端 dist 曾在 gate-coverage / gate-web / build-frontend 各编一次；Rust 又在
# 各 job 用独立 cache / debug profile 各编一套依赖图。C10 钉住这两处回归。
python3 - <<'PYEOF'
import sys
from pathlib import Path
try:
    import yaml
except ImportError:
    print('      缺 PyYAML：无法核对 release.yml')
    sys.exit(1)

yml = Path('.github/workflows/release.yml').read_text(encoding='utf-8')
code = [ln for ln in yml.splitlines() if not ln.lstrip().startswith('#')]
jobs = (yaml.safe_load(yml) or {}).get('jobs') or {}
bad = []

if sum('bun run build' in ln for ln in code) != 1:
    bad.append('可执行 `bun run build` 应只有 1 处')
if sum('name: web-dist' in ln and any('actions/upload-artifact' in x for x in code[max(0, i-4):i])
       for i, ln in enumerate(code)) != 1:
    bad.append('`web-dist` 上传步应只有 1 处')
# 注释里允许写旧 runner 作为反例，但可执行配置不许出现。
if any('macos-14' in ln for ln in code):
    bad.append('可执行配置里仍有 `macos-14`')

def steps_text(name):
    return '\n'.join(str(s.get('run') or '') + '\n' + str(s.get('with') or '')
                     for s in (jobs.get(name) or {}).get('steps') or [])

td = jobs.get('test-desktop') or {}
td_steps = td.get('steps') or []
td_text = steps_text('test-desktop')
if not td:
    bad.append('缺少 `test-desktop`：桌面真机验收不能省')
else:
    if 'cargo test --release -p node-connector --all-targets' not in td_text:
        bad.append('`test-desktop` 未跑 --release，会再编一套 debug 依赖图')
    if 'desktop-${{ matrix.target }}' not in td_text:
        bad.append('`test-desktop` 未复用 `build-desktop` 的 shared-key')
    cache = next((s for s in td_steps if str(s.get('uses') or '').startswith('Swatinem/rust-cache')), {})
    if (cache.get('with') or {}).get('save-if') not in (False, 'false'):
        bad.append('`test-desktop` 应 save-if: false，只读取缓存')
    if 'build-desktop' not in str(td.get('needs') or []):
        bad.append('`test-desktop` 应 needs build-desktop，保证缓存先写好')
    # v0.7.2 的 Windows job 挂了 53m51s 才被取消：没有 job 级超时时，
    # 一条阻塞的 `ReadFile` 会一直等到 GitHub 的 6 小时默认上限。
    timeout_minutes = td.get('timeout-minutes')
    if not isinstance(timeout_minutes, int) or isinstance(timeout_minutes, bool) or timeout_minutes <= 0:
        bad.append('`test-desktop` 缺 timeout-minutes（或不是正整数）：挂死要快速红')
# 用户 2026-11-07 裁定「CI 不需要 gnu 和 musl 两种吧？浪费资源」：gnu 产物
# 既不进 Release 也不进 `FROM scratch` 镜像，多编两套依赖图纯属白烧 runner。
bb = jobs.get('build-binaries') or {}
bb_targets = [str(x.get('target') or '') for x in ((bb.get('strategy') or {}).get('matrix') or {}).get('include') or []]
if not bb_targets or any(not t.endswith('-musl') for t in bb_targets):
    bad.append('`build-binaries` 应只编 musl 目标，gnu 已无消费方')
if 'gate-static' not in str((jobs.get('gate-test') or {}).get('needs') or []):
    bad.append('`gate-test` 应 needs gate-static，保证普通 Rust 缓存先写好')
for name in ('gate-static', 'gate-test', 'build-binaries', 'build-desktop'):
    if 'shared-key' not in steps_text(name):
        bad.append(f'`{name}` 未声明 shared-key，Rust 依赖图会各编一遍')
for b in bad:
    print(f'      {b}')
sys.exit(1 if bad else 0)
PYEOF
  [[ $? -ne 0 ]] && { echo "  ❌ release.yml 又出现重复编译 / 旧 macOS runner"; fail=1; } || ok "前端单点构建、Rust 缓存复用、桌面测试与 macos-latest 保留"

echo
echo "== C11 release.yml 不得出现 continue-on-error（含注释），cov-gate 必须硬阻断 =="
python3 - <<'PYEOF'
import sys
from pathlib import Path
try:
    import yaml
except ImportError:
    print('      缺 PyYAML：无法核对 release.yml')
    sys.exit(1)

yml = Path('.github/workflows/release.yml').read_text(encoding='utf-8')
jobs = (yaml.safe_load(yml) or {}).get('jobs') or {}
bad = []

# 注释里也不许留旧措辞，否则下次有人会照注释把 step 级豁免加回来。
if 'continue-on-error' in yml:
    bad.append('`release.yml` 不得出现 continue-on-error（含注释）：cov-gate 必须硬阻断')
coverage = jobs.get('gate-coverage') or {}
coverage_run = '\n'.join(str(s.get('run') or '') for s in (coverage.get('steps') or []))
if 'cargo llvm-cov --workspace --all-targets --summary-only --fail-under-lines 98' not in coverage_run:
    bad.append('`gate-coverage` 的 cov-gate 命令必须保持 --fail-under-lines 98')

for b in bad:
    print(f'      {b}')
sys.exit(1 if bad else 0)
PYEOF
  [[ $? -ne 0 ]] && { echo "  ❌ continue-on-error 回流或 cov-gate 命令被改"; fail=1; } || ok "release.yml 无 continue-on-error，cov-gate 仍以 98% 硬阻断"

echo
if [[ $fail -eq 0 ]]; then echo "门禁通过"; else echo "门禁未通过"; exit 1; fi
