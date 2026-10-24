#!/usr/bin/env bash
# 把 `cargo llvm-cov --show-missing-lines` 的 `Uncovered Lines:` 段展开成
# `路径:行号` 每行一条（已排序去重）。
#
# 为什么单独一个脚本：`scripts/check-coverage.sh`（门禁）和
# `scripts/coverage-exceptions.sh --rebuild`（例外清单重建）都要这个集合，
# 而它们必须给出**同一个**答案。抄两份管线的代价不是多写一遍，而是会漂移——
# llvm-cov 的输出格式一变（或我们想支持别的范围记号），改一处忘一处，
# 两个门禁就对同一份源码给出不同的未覆盖集合。
#
# 输入：stdin（llvm-cov 的完整输出），或 `--file <path>` 指定一个已存盘的文件。
# 输出：stdout，`路径:行号` 每行一条。
set -uo pipefail

src="${1:--}"
if [ "$src" != "-" ] && [ ! -f "$src" ]; then
    echo "FATAL: no such file: $src" >&2
    exit 2
fi

cat "$src" \
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
    done | sed "s|^$PWD/||" | LC_ALL=C sort -u
