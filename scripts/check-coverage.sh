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

# 抽取 "src/x.rs: 1, 2-3, 5" 形式，展开成 "path:line" 每行一条
actual="$(printf '%s\n' "$raw" \
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
    done | sed "s|^$PWD/||" | sort -u)"

declared="$(grep -vE '^\s*(#|$)' "$EXCEPTIONS" \
    | cut -d: -f1,2 \
    | sort -u)"

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
    if ! printf '%s\n' "$declared" | grep -qxF "$miss"; then
        echo "ERROR: uncovered line not declared in $EXCEPTIONS: $miss" >&2
        fail=1
    fi
done <<< "$actual"

# 登记了但已覆盖 → 例外过期，门禁失败（防止例外清单变成永久豁免）
while IFS= read -r d; do
    [ -z "$d" ] && continue
    if ! printf '%s\n' "$actual" | grep -qxF "$d"; then
        echo "ERROR: stale exception (line is now covered), remove from $EXCEPTIONS: $d" >&2
        fail=1
    fi
done <<< "$declared"

if [ "$fail" -ne 0 ]; then
    echo
    echo "COVERAGE GATE: FAIL" >&2
    exit 1
fi

echo "COVERAGE GATE: PASS (every uncovered line is a declared exception)"