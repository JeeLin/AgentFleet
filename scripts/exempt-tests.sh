#!/usr/bin/env bash
# 给每个 `#[cfg(test)] mod tests {` 插入测试专用 lint 豁免。
# 生产代码禁 unwrap/panic（workspace lints deny），测试代码不禁——
# 测试里失败就该炸，unwrap 的 panic 消息比 match 分支更好排查。
set -euo pipefail

EXEMPT='    // 测试代码豁免生产级 panic 禁令：测试里 unwrap 失败就是要立刻炸，
    // 而 panic 消息比 match 分支更能说明是哪一行、哪个值不对。
    // let_underscore_must_use / indexing_slicing 同理：测试里故意 `let _ =`
    // 丢弃一个错误来验证「失败不影响状态」，按索引取值也是常规写法。
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::let_underscore_must_use,
        clippy::indexing_slicing
    )]
'

for f in $(grep -rl "^mod tests {" crates/*/src/*.rs); do
    # 幂等：数模块级的 `#![allow` 头，已有就不重复插。
    # 注意匹配到 `#![allow` 即可——旧版模板是单行，新版是多行，
    # 只匹配 `($` 会把单行旧版漏掉，导致二次插入出 duplicated attribute。
    n=$(grep -c "^    #!\[allow" "$f" || true)
    if [ "$n" -gt 0 ]; then
        echo "skip (already has $n): $f"
        continue
    fi
    # 在 `mod tests {` 行后插入
    awk -v ins="$EXEMPT" '
        /^mod tests \{/ && !done { print; printf "%s", ins; done=1; next }
        { print }
    ' "$f" > "$f.tmp" && mv "$f.tmp" "$f"
    echo "patched: $f"
done
