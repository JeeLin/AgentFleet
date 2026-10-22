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
        --uncovered)
            shift
            UNCOVERED_FILE="${1:-}"
            [ -n "$UNCOVERED_FILE" ] || { echo "FATAL: --uncovered 需要一个文件路径" >&2; exit 2; }
            ;;
        -h | --help)
            sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
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
    printf '%s\n' "$raw" \
        | sed -n '/^Uncovered Lines:/,$p' \
        | grep -E '\.rs:' \
        | while IFS= read -r line; do
            file="${line%%:*}"
            rest="${line#*:}"
            nums="${rest// /}"
            IFS=',' read -ra parts <<< "$nums"
            for p in "${parts[@]}"; do
                if [[ "$p" == *-* ]]; then
                    a="${p%%-*}"; b="${p##*-}"
                    for ((i = a; i <= b; i++)); do
                        echo "$file:$i"
                    done
                elif [[ -n "$p" ]]; then
                    echo "$file:$p"
                fi
            done
        done | sed "s|^$PWD/||" | sort -u
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
    """
    blocks = []
    n = len(lines)
    for i, l in enumerate(lines):
        if 'coverage: excluded' not in l or not is_marker(l):
            continue
        j = i
        while j - 1 >= 0 and lines[j - 1].lstrip().startswith('//') \
                and 'coverage: excluded' not in lines[j - 1]:
            j -= 1
        k = i
        while k + 1 < n and lines[k + 1].lstrip().startswith('//') \
                and 'coverage: excluded' not in lines[k + 1]:
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
            s = s[2:]
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

if mode == '--rebuild':
    # 人工精修过的理由按「同一行」记住：行号没动就原样带回，行号动了就退回标注原文。
    curated_by_line = {}
    for path, line, reason in rows:
        markers, regions = info_for(path)
        derived = owner(markers, line)
        if derived is None and line in regions:
            derived = TEST_BRANCH
        if derived is None:
            continue
        if derived.strip() != reason.strip():
            curated_by_line[(path, line)] = reason.strip()
    out = []
    for item in sorted(uncovered, key=lambda s: (s.rpartition(':')[0], int(s.rpartition(':')[2]))):
        path, _, num = item.rpartition(':')
        line = int(num)
        markers, regions = info_for(path)
        derived = owner(markers, line)
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
    derived = owner(markers, line)
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
        analyse "$MODE" "$EXCEPTIONS" '-'
        ;;
    --rebuild)
        tmp="$(mktemp)"
        trap 'rm -f "$tmp"' EXIT
        collect_uncovered > "$tmp"
        if [ -z "$(grep -c . "$tmp")" ]; then
            echo "FATAL: no uncovered lines collected" >&2
            exit 1
        fi
        # 表头照抄现文件（表头是文件的一部分，不是生成物）
        sed -n '1,/^[^#]/p' "$EXCEPTIONS" | sed '$d' > "$tmp.header"
        cat "$tmp.header"
        analyse "$MODE" "$EXCEPTIONS" "$tmp"
        ;;
esac
