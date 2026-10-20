#!/usr/bin/env bash
# 本仓库的 grep 门禁。新增规则按 C 序号追加。
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0
ok()   { echo "  ✅ $1"; }
bad()  { echo "  ❌ $1"; fail=1; }

echo "== C1 子 crate 不得手写依赖版本号（必须收敛到 [workspace.dependencies]）=="
hits=$(grep -nE '^[[:space:]]*[a-zA-Z0-9_-]+[[:space:]]*=[[:space:]]*"[0-9]' crates/*/Cargo.toml || true)
if [[ -z "$hits" ]]; then ok "无手写版本号"; else bad "子 crate 出现手写版本号："; echo "$hits" | sed 's/^/      /'; fi

echo "== C2 设备二进制不得被称作 agent（约束 2）=="
# 与 C3 同口径排除 `-reports/`：审查报告为了证明「某模式零命中」会把正则
# 本身写进正文，那不是规格在把设备叫 agent。
hits=$(grep -rnE 'Dockerfile\.agent|agentfleet-agent|agent-binaries|docker-agent|agent 镜像' --include='*.md' docs/ .mdflow/ 2>/dev/null \
  | grep -v '/M1-reports/' || true)
if [[ -z "$hits" ]]; then ok "命名合规"; else bad "出现把设备二进制叫 agent 的用法："; echo "$hits" | sed 's/^/      /'; fi

echo "== C3 里程碑文档的 docs/ 锚点必须有效（A 轨 §N.N 解析到小节标题；B 轨 file.md:N 行号有效）=="
if compgen -G ".mdflow/milestones/*.md" > /dev/null; then
  python3 - <<'PYEOF'
import re, os, sys, glob

# 遍历 .mdflow/milestones/*.md，排除 -reports/ 子目录（审查报告不是被门禁的规格）
FILES = [p for p in sorted(glob.glob('.mdflow/milestones/*.md'))
         if not os.path.basename(p).startswith('-')]

# 已知的「别名 → 真文件名」映射。规范允许前缀省略（「设计规范」= design-system.md、
# 「protocol.md」= docs/architecture/protocol.md），故此处做前缀匹配而非全等。
ALIASES = {
    'design-system': 'docs/design-system.md',
    'protocol': 'docs/architecture/protocol.md',
    'PRODUCT': 'docs/PRODUCT.md',
    'DEVELOPMENT': 'docs/DEVELOPMENT.md',
    'DECISIONS': 'docs/DECISIONS.md',
    'BUGS': 'docs/BUGS.md',
    'AGENTS': 'AGENTS.md',
    'README': 'docs/architecture/README.md',
    'security': 'docs/architecture/security-and-lifecycle.md',
    'scheduling': 'docs/architecture/scheduling.md',
    'session-and-trajectory': 'docs/architecture/session-and-trajectory.md',
    'task-continuity': 'docs/architecture/task-continuity.md',
    'workspace-and-sync': 'docs/architecture/workspace-and-sync.md',
    'ai-gateway': 'docs/architecture/ai-gateway.md',
    'inference': 'docs/architecture/inference.md',
}
# 中文简称：歧义较大的（协议/产品/开发/决策）逐一显式钉死，不做猜测
ZH_ALIASES = {
    '设计规范': 'docs/design-system.md',
    '规范': 'docs/design-system.md',
    '协议': 'docs/architecture/protocol.md',
    '产品': 'docs/PRODUCT.md',
    '开发': 'docs/DEVELOPMENT.md',
    '决策': 'docs/DECISIONS.md',
}

# A 轨的 §N.N 若紧跟在这些扩展名之后（tokens.css §13 / app.js §x），指的是
# 原型资产或代码文件自己的编号系统，不是 docs 的节号，按测试标准明示除外。
EXTERNAL_SUFFIX = ('.css', '.js', '.ts', '.rs', '.json', '.sql')


def resolve(tok):
    """把一个文档标识（裸名 / 别名 / 路径）解析成仓库内路径；解析不出返回 None。

    裸名（不含 `/`）先查别名表：`README.md` 指 `docs/architecture/README.md` 而不是
    仓库根那个同名的 README——这是本仓库的既有用法（M0 文档 `README.md:41`「status 只有
    online/offline」说的就是数据模型那一行）。带路径的标识按存在性解析。
    """
    t = tok.strip('`').rstrip('.,;:、)）')
    if '/' not in t:
        base = os.path.basename(t)
        if base.endswith('.md'):
            base = base[:-3]
        if base in ALIASES and os.path.exists(ALIASES[base]):
            return ALIASES[base]
        if base in ZH_ALIASES:
            return ZH_ALIASES[base]
    for cand in (t, 'docs/' + t, 'docs/architecture/' + t):
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
    for i, line in enumerate(lines):
        m = re.match(r'^#{1,6}\s+(\d+(?:\.\d+)*)[.\s、]', line)
        if m:
            out.setdefault(m.group(1), i + 1)
    _HEAD_CACHE[path] = out
    return out


# 带 .md 的文档标识（允许路径前缀与前导 .）：`docs/x.md` / `x.md` / `.mdflow/…/x.md`
MD_PATH = r'\.?[A-Za-z0-9_][A-Za-z0-9_\-/\.]*\.md'
# 裸别名（不含 .md）：design-system / protocol / 设计规范 / 规范 / …
BARE_ALIAS = r'(?:design-system|protocol|PRODUCT|DEVELOPMENT|DECISIONS|BUGS|AGENTS|README|security|scheduling|设计规范|规范|协议)'
MENTION = re.compile(r'`?((?:' + MD_PATH + r')|(?:' + BARE_ALIAS + r')):?(?:\d+(?:-\d+)?)?`?')
SECTION = re.compile(r'§\s*(\d+(?:\.\d+)*)')
LINEANCHOR = re.compile(r'`?(' + MD_PATH + r'):(\d+)`?')

bad_a, bad_b = [], []
n_a = n_b = n_hist = 0

for path in FILES:
    for lineno, line in enumerate(open(path, encoding='utf-8').read().split('\n'), 1):
        # ---- B 轨：`xxx.md:N` 裸行号 ----
        # 只校验行号在文件行数范围内且该行非空。行号是本仓库已造成过漂移的引用形式
        # （DEVELOPMENT.md 是表格，插一行就全错位），不配当强校验。
        # 「原 `xxx.md:N`」指**历史上那一版**的行号（文件已改过），只查文件存在。
        historical = bool(re.search(r'原\s*`[^`]*\.md:\d+', line))
        for m in LINEANCHOR.finditer(line):
            rp = resolve(m.group(1))
            if rp is None:
                bad_b.append((path, lineno, m.group(1), int(m.group(2)), 'FILE MISSING'))
                continue
            if historical:
                n_hist += 1
                continue
            fl = open(rp, encoding='utf-8').read().split('\n')
            n = int(m.group(2))
            if n < 1 or n > len(fl) or not fl[n - 1].strip():
                bad_b.append((path, lineno, m.group(1), n, 'LINE EMPTY OR OUT OF RANGE'))
            else:
                n_b += 1
        # ---- A 轨：`§N.N` 必须解析到小节标题 ----
        # 归属 = 该 § 之前**同一行内**最近的显式文件名（允许前缀省略与中文简称）。
        # 同一行内没有可归属的文件名时不判（不误红）：跨行继承会把列表里更早出现的
        # 别的文件名误认成归属，实测会造成 12 处误报。
        # 指向原型资产/外部编号系统的 `§`（tokens.css §13 等）按测试标准明示除外。
        mentions = [(m.start(), resolve(m.group(1))) for m in MENTION.finditer(line)]
        mentions = [(pos, rp) for pos, rp in mentions if rp]
        for m in SECTION.finditer(line):
            # 该 § 紧跟在一个非 md 标识（原型资产 / 代码文件）之后 → 外部编号系统，跳过
            if line[:m.start()].rstrip().endswith(EXTERNAL_SUFFIX):
                continue
            sec = m.group(1)
            prev = [rp for pos, rp in mentions if pos < m.start()]
            if not prev:
                continue  # 无法判定归属 → 不误红
            owner = prev[-1]
            n_a += 1
            if sec not in headings(owner):
                bad_a.append((path, lineno, owner, sec, 'NOT A SECTION HEADING'))

print(f"  A 轨（§N.N 稳定锚）：{n_a} 处待解析，{'全部解析到小节标题' if not bad_a else str(len(bad_a)) + ' 处悬空'}")
print(f"  B 轨（file.md:N 裸行号）：{n_b + n_hist} 处（含 {n_hist} 处历史锚点仅查文件存在），"
      f"{'无失效' if not bad_b else str(len(bad_b)) + ' 处失效'}")
if bad_a:
    for p, ln, owner, sec, why in bad_a:
        print(f"  ❌ {p}:{ln} {owner} §{sec} {why}")
if bad_b:
    for p, ln, f, n, why in bad_b:
        print(f"  ❌ {p}:{ln} {f}:{n} {why}")
if bad_a or bad_b:
    sys.exit(1)
PYEOF
  [[ $? -ne 0 ]] && fail=1
else
  echo "  ⏭️  无里程碑文档，跳过"
fi

echo
if [[ $fail -eq 0 ]]; then echo "门禁通过"; else echo "门禁未通过"; exit 1; fi
