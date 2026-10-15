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
set -euo pipefail

cd "$(dirname "$0")/.."

EXCEPTIONS="coverage-exceptions.txt"

if [ ! -f "$EXCEPTIONS" ]; then
    echo "FATAL: missing $EXCEPTIONS" >&2
    exit 1
fi

echo "==> collecting uncovered lines"
raw="$(cargo llvm-cov --workspace --all-targets --show-missing-lines 2>/dev/null)" || {
    echo "FATAL: cargo llvm-cov failed" >&2
    exit 1
}

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