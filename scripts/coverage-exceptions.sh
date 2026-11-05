#!/usr/bin/env bash
# 覆盖率例外的「理由锚点」核对工具。
#
# 覆盖率门禁（scripts/check-coverage.sh）管的是**行集合**：每条未覆盖行都必须在
# coverage-exceptions.txt 里登记，每条登记都必须确实仍未覆盖。它不判断理由。
#
# 本工具管的是**理由的锚点**：每条登记的理由必须能在源码里找到出处——
# 要么同一文件里就近的 `// coverage: excluded — …` 就地标注，要么该行落在
# `#[cfg(test)]` 块内（测试自身的失败分支）。两者都不是的，说明这条理由是
# 「手写的、没有出处的」，登记它等于让门禁接受一条无法复核的理由。
#
# 用法：
#   scripts/coverage-exceptions.sh --check
#       核对每条登记的理由是否锚定在代码上。**不跑 cargo，秒级**，
#       可以进 CI 门禁链。
#
#   scripts/coverage-exceptions.sh --rebuild
#       按「就近标注优先、没有则测试块兜底」的推导口径重建整张表并打印到 stdout，
#       供人工对照。**需要跑一次 cargo llvm-cov（约 20 分钟）**。
#
#   scripts/coverage-exceptions.sh --rebuild --uncovered <file>
#       同上，但用已有的未覆盖行清单（`路径:行号` 每行一条）代替实跑，
#       便于离线对照。清单是调用方给的，工具会提示这一点。
set -uo pipefail

cd "$(dirname "$0")/.."

EXCEPTIONS="coverage-exceptions.txt"
MODE="--check"
UNCOVERED_FILE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --check) MODE="--check" ;;
        --rebuild) MODE="--rebuild" ;;
        --racy) MODE="--racy" ;;
        --uncovered)
            shift
            UNCOVERED_FILE="${1:-}"
            [ -n "$UNCOVERED_FILE" ] || { echo "FATAL: --uncovered 需要一个文件路径" >&2; exit 2; }
            ;;
        -h | --help)
            # 打到说明段末行（`set -uo pipefail` 前一行）：原写法 `2,22p` 少打了
            # `--uncovered` 的末句「清单是调用方给的，工具会提示这一点」，
            # 而那半句正是这个参数的安全边界。
            sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "未知参数：$1（--help 看用法）" >&2
            exit 2
            ;;
    esac
    shift
done

if [ ! -f "$EXCEPTIONS" ]; then
    echo "FATAL: missing $EXCEPTIONS" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# 收集未覆盖行
# ---------------------------------------------------------------------------
collect_uncovered() {
    if [ -n "$UNCOVERED_FILE" ]; then
        echo "==> using caller-supplied uncovered list: $UNCOVERED_FILE" >&2
        grep -vE '^\s*(#|$)' "$UNCOVERED_FILE"
        return
    fi
    echo "==> collecting uncovered lines (cargo llvm-cov)" >&2
    local raw err
    err="$(mktemp)"
    raw="$(cargo llvm-cov --workspace --all-targets --show-missing-lines 2>"$err")"
    local status=$?
    if [ "$status" -ne 0 ]; then
        echo "FATAL: cargo llvm-cov failed (exit $status)" >&2
        cat "$err" >&2
        rm -f "$err"
        exit 1
    fi
    rm -f "$err"
    # 展开成 `路径:行号` 的管线只有一份，在 scripts/uncovered-lines.sh 里：
    # 本工具与 scripts/check-coverage.sh 都要这个集合，抄两份必然漂移。
    printf '%s\n' "$raw" | "$(dirname "$0")/uncovered-lines.sh"
}

# ---------------------------------------------------------------------------
# 锚点推导（Python：注释块与 `#[cfg(test)]` 区间用脚本做太脆）
# ---------------------------------------------------------------------------
analyse() {
    python3 - "$1" "$2" "$3" <<'PYEOF'
import os
import re
import sys

mode, exceptions_path, uncovered_path = sys.argv[1], sys.argv[2], sys.argv[3]
TOKEN = re.compile(r'coverage: excluded\s*(?:—+\s*)?')
TEST_BRANCH = '测试自身的失败分支（`expect` / `assert!` 的消息行）：绿的时候永远不执行'


def read_lines(path):
    with open(path, encoding='utf-8') as f:
        return f.read().split('\n')


def is_marker(line):
    """这一行是不是一条**标注本身**，而不是「在散文里提到这个词」。

    文档注释（`//!`）里常常写「已就地标注 `coverage: excluded`」这类说明——
    那是**关于标注的说明**，不是标注。把它当标注会凭空造出孤儿条目。
    判据：`//`（或行尾 `//`）之后紧跟 `coverage: excluded`。
    """
    idx = line.find('//')
    while idx >= 0:
        rest = line[idx + 2:].lstrip()
        if rest.startswith('coverage: excluded'):
            return True
        idx = line.find('//', idx + 2)
    return False


def comment_blocks(lines):
    """标注所在的注释段 -> [(start1, end1)]（1-based，闭区间）。

    标注可以写在行尾（`} // coverage: excluded — …`），所以不能只看「整行是注释」。
    一段注释 = 标注行本身 + 它前面紧邻的整行注释（不含别的标注）
    + 它后面紧邻的整行注释（不含别的标注）。

    扩块时必须用 `is_marker` 而不是子串匹配：散文注释（如 exec.rs:580-589
    解释「为什么标 racy」的那段）里会**提到** `coverage: excluded` 这个词，
    子串匹配会把那段散文并进标注块，两个后果——
    ① 真正的行尾标注（exec.rs:590）被吸进散文块，块判定不再是「单行行尾
       标注」，辖区从 590 错到 591（`}`），racy 剔除漏掉 590 → 门禁报
       uncovered not declared（CI 实测红过一次）；
    ② marker_line 取「块内第一个含子串的行」（散文行 584）而不是真标注行，
       归属与理由提取全部错位。
    """
    blocks = []
    n = len(lines)
    for i, l in enumerate(lines):
        if 'coverage: excluded' not in l or not is_marker(l):
            continue
        j = i
        while j - 1 >= 0 and lines[j - 1].lstrip().startswith('//') \
                and not is_marker(lines[j - 1]):
            j -= 1
        k = i
        while k + 1 < n and lines[k + 1].lstrip().startswith('//') \
                and not is_marker(lines[k + 1]):
            k += 1
        blocks.append((j + 1, k + 1))
    return blocks


def strip_comment(lines, start, end, marker_line):
    parts = []
    for i in range(start, end + 1):
        raw = lines[i - 1]
        s = raw.lstrip()
        if i == marker_line:
            cut = raw.find('coverage: excluded')
            pos = raw.rfind('//', 0, cut) if cut >= 0 else -1
            s = raw[pos + 2:].lstrip() if pos >= 0 else ''
            if s.startswith('/'):
                s = s.lstrip('/')
            if s.startswith(' '):
                s = s[1:]
            s = TOKEN.sub('', s, count=1)
        else:
            if not s.startswith('//'):
                continue
            # 文档注释是 `///`，只剥两个斜杠会把第三个留在理由开头（`/ xxx`），
            # 读起来像残缺。统一剥掉全部前导斜杠与紧随的一个空格。
            s = s.lstrip('/')
            if s.startswith(' '):
                s = s[1:]
        if s:
            parts.append(s)
    return ' '.join(parts)


def marker_blocks(lines):
    out = []
    for start, end in comment_blocks(lines):
        marker_line = None
        for i in range(start, end + 1):
            if 'coverage: excluded' in lines[i - 1]:
                marker_line = i
                break
        if marker_line is None:
            continue
        out.append((marker_line, strip_comment(lines, start, end, marker_line).strip()))
    return out


def test_regions(lines):
    regions = set()
    n = len(lines)
    for i, l in enumerate(lines):
        if l.lstrip().startswith('#[cfg(test)]'):
            depth = 0
            started = False
            for j in range(i, n):
                depth += lines[j].count('{') - lines[j].count('}')
                if '{' in lines[j]:
                    started = True
                regions.add(j + 1)
                if started and depth <= 0:
                    break
    return regions


def owner_marker(markers, line):
    """离 line 最近的标注所在行号：先向上取（最大者），没有再向下（最小者）。"""
    above = [m for m in markers if m[0] < line]
    if above:
        return max(above, key=lambda m: m[0])[0]
    below = [m for m in markers if m[0] > line]
    if below:
        return min(below, key=lambda m: m[0])[0]
    return None


def owner_outside_tests(markers, regions, line):
    """与 `owner` 同义，但**测试块内的行不认生产代码里的标注**。

    为什么需要这一层：`owner` 的规则是「先向上取最近的标注，取不到再向下」。
    对一条落在 `#[cfg(test)]` 模块里的未覆盖行，向上取会跨过 `mod tests {`
    的边界，捞到上面某个生产函数的标注——于是这条登记的理由写成
    「线程里这一臂不可达」，而被豁免的其实是一句 `other => panic!(…)`
    的测试兜底臂。`--check` 只看「有没有锚点」，错锚同样能通过，缺陷因此
    长期隐形（实测全仓 620 条曾 100% 被判为「锚在就地标注」、测试块一栏为 0，
    而其中两百多条实际位于测试模块内）。

    判据：行在测试区间内时，只接受**同样落在测试区间内**的标注；生产区的
    标注对它无效。测试区间内没有标注就返回 None，由调用方退回
    `TEST_BRANCH`——那句理由（「绿的时候永远不执行」）才是这些行的实情。
    """
    if line in regions:
        return owner([m for m in markers if m[0] in regions], line)
    return owner(markers, line)


def owner(markers, line):
    mk = owner_marker(markers, line)
    for m in markers:
        if m[0] == mk:
            return m[1]
    return None


def marker_files():
    """crates/ 下带就地标注的源码文件（按路径排序）。"""
    out = []
    for root, dirs, names in os.walk('crates'):
        dirs[:] = [d for d in dirs if d != 'target']
        for name in sorted(names):
            if not name.endswith('.rs'):
                continue
            p = os.path.join(root, name)
            try:
                with open(p, encoding='utf-8') as f:
                    if 'coverage: excluded' in f.read():
                        out.append(p)
            except OSError:
                continue
    return sorted(out)


cache = {}


def info_for(path):
    if path not in cache:
        lines = read_lines(path)
        cache[path] = (marker_blocks(lines), test_regions(lines))
    return cache[path]


rows = []
with open(exceptions_path, encoding='utf-8') as f:
    for raw in f:
        raw = raw.rstrip('\n')
        if not raw.strip() or raw.lstrip().startswith('#'):
            continue
        head, _, reason = raw.partition(': ')
        path, _, num = head.rpartition(':')
        rows.append((path, int(num), reason))

uncovered = set()
if uncovered_path and uncovered_path != '-':
    with open(uncovered_path, encoding='utf-8') as f:
        for raw in f:
            raw = raw.strip()
            if raw:
                uncovered.add(raw)

if mode == '--racy':
    # 输出**覆盖性非确定**的行集合：源码里带 `// coverage: excluded (racy)`
    # 就地标注的那些。语义与其余例外同规则——标注就写在被豁免的那一行上
    # （行尾或紧邻其上的注释段），归属由同一个 `marker_blocks` 判定，不另立一套。
    #
    # 消费者是 `scripts/check-coverage.sh`：它把这些行从「集合相等」的两个方向
    # 都比对里剔掉。之所以要单独一类，是因为 `tokio::select!` 对多条就绪的臂
    # **随机轮询**，臂内代码的执行取决于协程调度时序，同一次代码每次 llvm-cov
    # 实跑的结果都可能不同（实测同一份代码四次实跑报错 8 / 3 / 34 条，且集合
    # 成员互不重合）。按确定性行核对它，门禁会随机红——而随机红的门禁等于没有
    # 门禁，开发者无法区分「我改坏了」与「这次运气不好」。
    # 「racy 行」= 标注段所辖的**代码行**，不是标注行本身。
    #
    # 为什么不能只报标注行：llvm-cov 报的未覆盖行永远是**代码行**（`}`、
    # `sleep(...)` 这类），而标注写在注释里。只报标注行的后果是——actual 侧
    # 永远没有 821 这一行，`grep -vxF` 剔除是空操作，racy 机制形同虚设；
    # 这些代码行会以 racy 理由文本被登记进例外表，等某次实跑覆盖了它们，
    # 门禁报 stale exception——CI 首跑实测就是这样红的（acp.rs:820/828）。
    #
    # 辖区判定——分三种标注形态，收法不同：
    # 1) 行尾标注（`child_exec(...) // coverage: excluded (racy)`）：只辖标注
    #    所在的那**一行代码**。判据是 `//` 之前有非空白内容，**不看块大小**——
    #    行尾标注上方常紧贴解释用的散文注释块（exec.rs:580-589 整段解释
    #    「为什么标 racy」，物理上与 590 的行尾标注连续），按块大小判定会把
    #    590 吸进散文块、辖区错到下一行（实测让门禁红过）。
    # 2) 独立注释块：`break; }` 这类短臂的代码可能整段写在注释块**上方**
    #    （实测 acp.rs:819-820），只往下找会漏——所以下方取连续代码行、
    #    上方也取紧邻代码行，遇空行即停。
    def racy_code_lines(path, lines):
        out = []
        for start, end in comment_blocks(lines):
            marker_line = next((i for i in range(start, end + 1)
                                if 'coverage: excluded' in lines[i - 1]), None)
            if marker_line is None:
                continue
            reason = strip_comment(lines, start, end, marker_line)
            if '(racy)' not in reason:
                continue
            code_before_marker = lines[marker_line - 1].split('//')[0].strip()
            if code_before_marker:
                # 形态 1：行尾标注 → 只辖本行
                out.append('%s:%d' % (path, marker_line))
                continue
            # 形态 2：独立注释块 → 下方连续代码行 + 上方**仅紧邻一行**。
            # 上方多收会把无关的前序语句拉进来（detect.rs:362 标注向上穿
            # 过 `Err(e) => {` 后把 355-360 的超时臂代码全收了——那些行
            # 是确定可达的，进 racy 集等于放走真未覆盖）。上方紧邻一行
            # 够覆盖「短臂写在注释上方」的实测形态（acp.rs:819-820），
            # 因为那种臂的上一行是 `break`/单语句，不会更长。
            i = end + 1
            while i <= len(lines):
                s = lines[i - 1].strip()
                if not s or s.startswith('//'):
                    break
                out.append('%s:%d' % (path, i))
                i += 1
            if start - 1 >= 1:
                s = lines[start - 2].strip()
                if s and not s.startswith('//'):
                    out.append('%s:%d' % (path, start - 1))
        return out

    racy = []
    for p in marker_files():
        racy.extend(racy_code_lines(p, read_lines(p)))
    print('\n'.join(sorted(racy)))
    sys.exit(0)

if mode == '--rebuild':
    # 人工精修过的理由按「同一行 + 同一归属类别」记住。
    #
    # 只按行号记是不够的：行号没动但**这一行的身份变了**（比如上一次重建把一条
    # 测试兜底臂错锚到生产标注上、写成「线程里这一臂不可达」，这次规则修好、
    # 它应当落回 `TEST_BRANCH`），只比对行号就会把那条错理由原样搬回来，
    # 于是规则修好了而表还是错的——实测 `git.rs:1382` 正是这样活下来的。
    # 判据：旧理由要么等于旧规则的推导（说明它是生成的、不是精修的），要么
    # 与新规则推导**同类**（同为 TEST_BRANCH，或同为标注原文的语义域）。
    # 简单可靠的做法：只有「旧理由的类别」== 「新推导的类别」时才带回。
    def category(reason, test_branch):
        if reason.strip() == test_branch:
            return 'test'
        return 'marker'

    curated_by_line = {}
    for path, line, reason in rows:
        markers, regions = info_for(path)
        derived = owner_outside_tests(markers, regions, line)
        if derived is None and line in regions:
            derived = TEST_BRANCH
        if derived is None:
            continue
        if derived.strip() != reason.strip():
            # 类别不一致 → 不回带：那一行的性质变了，旧理由说的是另一件事。
            if category(reason, TEST_BRANCH) != category(derived, TEST_BRANCH):
                continue
            curated_by_line[(path, line)] = reason.strip()
    out = []
    # racy 行必须从重建结果里剔除：`check-coverage.sh` 把它们从**两个方向**的
    # 集合比对里都减掉，于是「登记表里有它」这件事永远对不上——actual 侧没有、
    # declared 侧有，门禁每次都报同一堆 stale exception。那不是「这次运气不好」，
    # 是先天地无法满足的命题；让它进表 = 让门禁永久红。
    racy_lines = set()
    for p in marker_files():
        for marker_line, text in marker_blocks(read_lines(p)):
            if '(racy)' in text:
                racy_lines.add('%s:%d' % (p, marker_line))
    for item in sorted(uncovered, key=lambda s: (s.rpartition(':')[0], int(s.rpartition(':')[2]))):
        if item in racy_lines:
            continue
        path, _, num = item.rpartition(':')
        line = int(num)
        markers, regions = info_for(path)
        derived = owner_outside_tests(markers, regions, line)
        if derived is None and line in regions:
            derived = TEST_BRANCH
        if derived is None:
            sys.stderr.write('UNDECIDED %s\n' % item)
            continue
        reason = curated_by_line.get((path, line), derived.strip())
        out.append('%s:%d: %s' % (path, line, reason))
    print('\n'.join(out))
    sys.exit(0)

# --check
unanchored = []
curated = []
by_marker = 0
by_test = 0
for path, line, reason in rows:
    if not path.startswith('crates/'):
        unanchored.append((path, line, reason, '登记的文件不在 crates/ 下'))
        continue
    markers, regions = info_for(path)
    derived = owner_outside_tests(markers, regions, line)
    if derived is not None:
        by_marker += 1
        if derived.strip() != reason.strip():
            curated.append((path, line, reason, derived))
    elif line in regions:
        by_test += 1
    else:
        unanchored.append((path, line, reason, '就近没有 `coverage: excluded` 标注，该行也不在 `#[cfg(test)]` 块内'))

# 反向：就地标注 → 清单。一条标注如果没有对应的登记行，就是孤儿标注：
# 它声称「这里无法触达」，却没有任何一行真的因此被豁免——要么理由不成立
# （那几行其实已被覆盖），要么登记漏了。两者都是门禁该拦住的。
#
# 例外：`#[cfg(test)]` 块内的标注不参与孤儿判定。测试代码里写
# 「这个 assert 只在 pipe2 失败时有意义」是**解释性说明**，不是申请豁免——
# 测试模块压根不需要登记例外（它本来就不在「 coverage 必须100% 」的生产口径里）。
# 要求它们每条都对应一行登记，只会逼人在之后删掉这些有用的解释。
orphans = []
by_path = {}
for path, line, reason in rows:
    by_path.setdefault(path, []).append(line)
mf = marker_files()
for path in mf:
    markers, regions = info_for(path)
    claimed = by_path.get(path, [])
    used = {owner_marker(markers, line) for line in claimed}
    for marker_line, text in markers:
        if marker_line in regions:
            continue
        # `(racy)` 标注不参与孤儿判定：它申请的不是「这一行不可触达」的例外，
        # 而是「这一行的覆盖性非确定」——由 `check-coverage.sh` 单独从双向
        # 比对里豁免（见 `--racy`）。把它当普通例外要求一条登记行，会让
        # 「非确定」被写成「不可达」，而那是不诚实的原因。
        if '(racy)' in text:
            continue
        if marker_line not in used:
            orphans.append((path, marker_line, text))

print('登记条数：%d' % len(rows))
print('  理由锚在就近的就地标注：%d' % by_marker)
print('  理由锚在测试块（测试自身的失败分支）：%d' % by_test)
print('  人工精修过的理由（文本与标注不同，内容更具体，保留）：%d' % len(curated))
print('  源码里的就地标注：%d 条（其中孤儿 %d 条）' % (len(mf) and sum(len(info_for(p)[0]) for p in mf), len(orphans)))
if curated:
    for path, line, reason, derived in curated:
        print('    %s:%d' % (path, line))
        print('      登记: %s' % reason[:120])
        print('      标注: %s' % derived[:120])
if orphans:
    print('')
    print('孤儿就地标注（没有对应登记行，说明理由不成立或登记漏了）：%d' % len(orphans))
    for path, marker_line, text in orphans:
        print('  ERROR: %s:%d — %s' % (path, marker_line, text[:100]))
if unanchored or orphans:
    print('')
    if unanchored:
        print('理由没有锚点的登记（门禁不该接受）：%d' % len(unanchored))
        for path, line, reason, why in unanchored:
            print('  ERROR: %s:%d — %s' % (path, line, why))
    print('')
    print('ANCHOR CHECK: FAIL')
    sys.exit(1)
print('')
print('ANCHOR CHECK: PASS (every exception reason is anchored, every marker is claimed)')
PYEOF
}

case "$MODE" in
    --check)
        # `--check` 走不到 `--uncovered`：它的语义就是「拿登记文件核对」，没有
        # 「少查一部分」的入口。显式拒绝而不是静默忽略——静默忽略会让
        # 「我以为只核了这几条」和「其实全核了」无法区分。
        if [ -n "$UNCOVERED_FILE" ]; then
            echo "FATAL: --check 不接受 --uncovered（--check 核对的是整张登记表）" >&2
            exit 2
        fi
        analyse "$MODE" "$EXCEPTIONS" '-'
        ;;
    --racy)
        analyse "$MODE" "$EXCEPTIONS" '-'
        ;;
    --rebuild)
        tmp="$(mktemp)"
        trap 'rm -f "$tmp" "$tmp.header"' EXIT
        collect_uncovered > "$tmp"
        # 表头照抄现文件（表头是文件的一部分，不是生成物）。不能用
        # `sed -n '1,/^[^#]/p' | sed '$d'`：那个写法把「第一个非 # 行」连同
        # 它的替换一起吃掉，表头末尾若是半句话就被静默截断——本仓实测出现过
        # 表头最后一行停在「本文件写下时写的是」，读者看不出缺了什么。
        awk '/^#/ { print; next } { exit }' "$EXCEPTIONS" > "$tmp.header"
        cat "$tmp.header"
        analyse "$MODE" "$EXCEPTIONS" "$tmp"
        ;;
esac
