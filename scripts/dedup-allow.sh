#!/usr/bin/env bash
# 清理 `mod tests` 内被重复插入的 `#![allow(...)]` 头，只保留第一份完整版。
# 成因：scripts/exempt-tests.sh 早期版本的幂等检测漏了单行旧模板，
# 二次运行会把 allow 头再插一遍，触发 clippy 的 duplicated_attribute。
set -euo pipefail

# 完整版 allow 头的特征行（按此判定「这份是完整版」）
FULL_MARK='clippy::indexing_slicing'

for f in $(grep -rl "^mod tests {" crates/*/src/*.rs); do
    # 数一下这个文件里有几个模块级 allow 头
    n=$(grep -c "^    #!\[allow" "$f" || true)
    [ "$n" -le 1 ] && continue

    echo "cleaning $f (has $n allow headers)"
    python3 - "$f" <<'PY'
import re, sys
path = sys.argv[1]
src = open(path, encoding="utf-8").read()

# 一个 allow 头 = 若干注释行 + 一个 `#![allow( ... )]`（单行或多行）
# 策略：按行扫，遇到 `    #![allow` 开始，收集到配平的 `)]` 结束，
# 连同其上方紧邻的注释行视为一个「块」；只保留第一个完整块。
lines = src.split("\n")
out, i, kept = [], 0, 0
while i < len(lines):
    ln = lines[i]
    if ln.startswith("    #![allow"):
        # 往前吃掉紧邻的注释行（已写入 out 的尾部）
        comments = []
        while out and out[-1].lstrip().startswith("//"):
            comments.insert(0, out.pop())
        # 收集整个 allow 属性
        block = [ln]
        depth = ln.count("(") - ln.count(")")
        j = i + 1
        while depth > 0 and j < len(lines):
            block.append(lines[j])
            depth += lines[j].count("(") - lines[j].count(")")
            j += 1
        text = "\n".join(block)
        is_full = "indexing_slicing" in text
        if kept == 0:
            out.extend(comments)
            out.extend(block)
            kept += 1
            if not is_full:
                print(f"  WARN: kept a non-full allow header in {path}", file=sys.stderr)
        else:
            # 丢弃后续块；若后续块才是完整版，用完整版替换已保留的
            if is_full and "indexing_slicing" not in "\n".join(out):
                for k in range(len(out) - 1, -1, -1):
                    if out[k].startswith("    #![allow"):
                        del out[k:]
                        break
                out.extend(comments)
                out.extend(block)
                kept = 1
        i = j
        continue
    out.append(ln)
    i += 1

open(path, "w", encoding="utf-8").write("\n".join(out))
PY
done
echo "done"
