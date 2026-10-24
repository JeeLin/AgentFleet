#!/usr/bin/env bash
# 覆盖率门禁校验。
#
# 用法：scripts/check-coverage.sh
#
# 逻辑：
#   1. 跑 `cargo llvm-cov --workspace --all-targets --show-missing-lines` 取未覆盖行。
#   2. 每一条未覆盖行必须出现在 coverage-exceptions.txt 中，且该文件里登记的
#      行也必须**确实**仍未覆盖（登记了却已覆盖 = 过期的例外，要被发现）。
#   3. 两边完全一致才算通过。
#
# 这不是「把覆盖率做高」的技巧：例外必须逐条写明理由，脚本拒绝任何
# 没登记的未覆盖行，也拒绝任何已失效的登记行。
set -uo pipefail

cd "$(dirname "$0")/.."

# 不接受任何参数：输入全由仓库固定（coverage-exceptions.txt + 当前源码树）。
# 能传参就意味着「可以少查一部分」，那条路走下去门禁会被悄悄放松。
for arg in "$@"; do
  case "$arg" in
    -h | --help)
      sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "未知参数：$arg（--help 看用法）" >&2
      exit 2
      ;;
  esac
done

EXCEPTIONS="coverage-exceptions.txt"

if [ ! -f "$EXCEPTIONS" ]; then
    echo "FATAL: missing $EXCEPTIONS" >&2
    exit 1
fi

# llvm-cov 需要一个独立的 target 目录（约 3–5G）。磁盘紧张时写满的报错
# 是链接器崩溃（Bus error / "database or disk is full"），看起来像代码问题，
# 实则是空间问题——所以先清掉上一轮的覆盖率产物再跑。
echo "==> reclaiming space from previous coverage runs"
rm -rf target/llvm-cov-target
avail_kb=$(df -Pk . | awk 'NR==2 {print $4}')
# 6G 是实测下限：workspace 全量插桩 + 链接峰值需要这个量
if [ "$avail_kb" -lt 6291456 ]; then
    echo "FATAL: only $((avail_kb / 1024))MiB free; llvm-cov needs >= 6144MiB." >&2
    echo "       Free some space (other projects' target/ dirs are the usual culprit)" >&2
    exit 1
fi

echo "==> collecting uncovered lines"
# 诊断不吞：编译错误 / 缺 llvm-tools / 参数错误全在 stderr 里，
# 丢掉它只剩一句 "FATAL: cargo llvm-cov failed"，排障者无从下手。
# 成功时把 stderr 收进临时文件（llvm-cov 的正常输出很吵），失败时原样转发。
llvm_cov_err="$(mktemp)"
trap 'rm -f "$llvm_cov_err"' EXIT
raw="$(cargo llvm-cov --workspace --all-targets --show-missing-lines 2>"$llvm_cov_err")"
llvm_cov_status=$?
if [ "$llvm_cov_status" -ne 0 ]; then
    echo "FATAL: cargo llvm-cov failed (exit $llvm_cov_status)" >&2
    echo "----- cargo llvm-cov stderr -----" >&2
    cat "$llvm_cov_err" >&2
    echo "--------------------------------" >&2
    exit 1
fi

# 抽取 "src/x.rs: 1, 2-3, 5" 形式，展开成 "path:line" 每行一条。
#
# 这段管线只有一份实现，在 scripts/uncovered-lines.sh 里：本门禁与
# `scripts/coverage-exceptions.sh --rebuild` 都要这个集合，而它们必须给出
# 同一个答案。抄两份的代价不是多写一遍，而是会漂移——llvm-cov 的输出格式
# 一变（或我们想支持别的范围记号），改一处忘一处，两个门禁就对同一份源码
# 给出不同的未覆盖集合。
actual="$(printf '%s\n' "$raw" | "$(dirname "$0")/uncovered-lines.sh")"

declared="$(grep -vE '^\s*(#|$)' "$EXCEPTIONS" \
    | cut -d: -f1,2 \
    | LC_ALL=C sort -u)"

# 覆盖性**非确定**的行：源码里带 `// coverage: excluded (racy)` 标注的那些。
#
# 为什么要有这一类：`tokio::select!` 对多条就绪的臂是**随机轮询**的，于是
# 臂内代码的执行取决于协程调度时序——同一份代码、同一个输入，每次 llvm-cov
# 实跑的结果都可能不同（实测同一次代码四次实跑得到 8 / 3 / 34 条报错，
# 且集合成员互不重合）。把这类行当成确定性行做「集合相等」双向核对，
# 门禁会随机红，而随机红的门禁等于没有门禁：开发者无法区分
# 「我改坏了」与「这次运气不好」。
#
# 这不是放水：非时序敏感行仍走严格双向比对，一条都不放松。被豁免的只是
# 「断言它某次一定不被执行」这个**不可能成立**的要求。判据改由该臂所属
# 函数自己的用例给（例：`select!` 的 pump 臂由 `channel::pump` 的用例覆盖）。
#
# 标注必须在源码里就地写（与其余例外同规则），因为「哪一行是 racy」是
# 代码结构的事实，不是清单能单方面声明的。
racy="$(scripts/coverage-exceptions.sh --racy)"

if [ -n "$racy" ]; then
    echo
    echo "==> non-deterministic lines (marked \`(racy)\` in source, excluded from both checks)"
    printf '%s\n' "$racy" | sed 's/^/  /'
fi

# 从两个方向都比对里剔掉 racy 行。
#
# 用 `grep -vxF -f`（整行精确、固定串）而**不是** `comm -13`：`comm` 要求两个
# 输入各自按**它认可的同一 collation** 排好序，否则它会在 stderr 打一行
# `input is not in sorted order` 之后**静默丢行**——结果集看着正常，却少了几条，
# 于是「两边都在」的行会被误判。这不是理论风险：本仓三份数据的排序实现本来就
# 不同（`$declared`/`$actual` 走 shell `sort`，受 `LC_ALL` 影响；`$racy` 走
# Python 的 `sorted()`，纯 codepoint 序），而 locale 排序把
# `connector/terminal.rs` 排在 `conn.rs` 之前、codepoint 序相反。用 `comm`
# 就把「门禁的判定」绑在一个与命题无关的排序细节上。
# `grep -vxF -f` 不要求有序，也不会因序不同而少输出，语义正是我们要的
# 「从集合里减掉另一个集合」。
if [ -n "$racy" ]; then
    declared_checked="$(printf '%s\n' "$declared" | grep -vxF -f <(printf '%s\n' "$racy") | sed '/^$/d')"
    actual_checked="$(printf '%s\n' "$actual" | grep -vxF -f <(printf '%s\n' "$racy") | sed '/^$/d')"
else
    declared_checked="$(printf '%s\n' "$declared" | sed '/^$/d')"
    actual_checked="$(printf '%s\n' "$actual" | sed '/^$/d')"
fi

echo
echo "==> declared exceptions (from $EXCEPTIONS)"
printf '%s\n' "$declared" | sed 's/^/  /'
echo "==> actually uncovered"
if [ -z "$actual" ]; then
    echo "  (none)"
else
    printf '%s\n' "$actual" | sed 's/^/  /'
fi
echo

fail=0

# 未覆盖但没登记 → 门禁失败
while IFS= read -r miss; do
    [ -z "$miss" ] && continue
    if ! printf '%s\n' "$declared_checked" | grep -qxF "$miss"; then
        echo "ERROR: uncovered line not declared in $EXCEPTIONS: $miss" >&2
        fail=1
    fi
done <<< "$actual_checked"

# 登记了但已覆盖 → 例外过期，门禁失败（防止例外清单变成永久豁免）
while IFS= read -r d; do
    [ -z "$d" ] && continue
    if ! printf '%s\n' "$actual_checked" | grep -qxF "$d"; then
        echo "ERROR: stale exception (line is now covered), remove from $EXCEPTIONS: $d" >&2
        fail=1
    fi
done <<< "$declared_checked"

if [ "$fail" -ne 0 ]; then
    echo
    echo "COVERAGE GATE: FAIL" >&2
    exit 1
fi

echo "COVERAGE GATE: PASS (every uncovered line is a declared exception)"