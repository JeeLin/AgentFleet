#!/usr/bin/env bash
# ============================================================================
#  按**用例全名集合**比较两次测试，而不是比总数。
# ============================================================================
#
#  为什么需要它（v0.8.1 G20）：里程碑文档与 DEVELOPMENT.md 反复拿「用例总数」
#  当判据（「开工前记 N，完成后 ≥ N」「零删除、零跳过」）。但**比总数时
#  「删 9 条加 9 条」也是绿的**——总数没变，覆盖的东西换了一批。而且同一个
#  `git status` 在不同时刻可能给出不同工作树（并发改、stash 进出），总数会
#  在 1198 ↔ 1207 之间跳，这条判据根本站不住。
#
#  用法：
#    scripts/test-set-diff.sh snapshot <out.json>   # 记一份基线
#    scripts/test-set-diff.sh compare  <base.json>  # 与基线比，缺一条即红
#
#  判据：**基线集合 ⊆ 当前集合**。少一条就用完整用例名打出来并退出 1。
#  新增用例不判红（那是正常开发），重命名算「旧的没了」——这点要留意。
#
#  退出码：0 = 没有用例消失；1 = 有用例消失（逐条打印）；2 = 用法/环境错误。
# ============================================================================
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WEB="$REPO/packages/web"

if [ ! -d "$WEB" ]; then
  echo "test-set-diff: 找不到 $WEB" >&2
  exit 2
fi

# 用 vitest 的 json reporter 取每条用例的 fullName，排序去重后写盘。
# fullName 是 describe 链 + it 标题拼起来的完整名，比名字更稳。
emit() {
  local out="$1" raw
  raw="$(mktemp)"
  ( cd "$WEB" && npx vitest run --reporter=json --outputFile="$raw" >/dev/null 2>&1 )
  if [ ! -s "$raw" ]; then
    echo "test-set-diff: vitest 没有产出 JSON，测试可能起不来" >&2
    rm -f "$raw"
    return 1
  fi
  node -e '
    const j = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const names = new Set();
    for (const f of j.testResults ?? [])
      for (const a of f.assertionResults ?? []) names.add(a.fullName);
    // 按**字节序**排，不用 localeCompare：`comm` 要求严格字节序，
    // 两者不一致时会报 "file is not in sorted order" 并可能漏判。
    console.log([...names].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0)).join("\n"));
  ' "$raw" > "$out"
  rm -f "$raw"
  wc -l < "$out" | tr -d ' '
}

case "${1:-}" in
  snapshot)
    dest="${2:-}"
    [ -n "$dest" ] || { echo "用法: $0 snapshot <out.json>" >&2; exit 2; }
    n="$(emit "$dest")" || exit 2
    echo "已记录 $n 条用例 → $dest"
    ;;
  compare)
    base="${2:-}"
    [ -n "$base" ] && [ -f "$base" ] || { echo "用法: $0 compare <base.json>（文件不存在）" >&2; exit 2; }
    now="$(mktemp)"
    n="$(emit "$now")" || { rm -f "$now"; exit 2; }
    # comm -23：只在基线里、不在当前的用例。LC_ALL=C 与上面的字节序排序对齐。
    missing="$(LC_ALL=C comm -23 "$base" "$now")"
    if [ -n "$missing" ]; then
      echo "❌ 有 $(printf '%s\n' "$missing" | wc -l | tr -d ' ') 条用例消失了（当前 $n 条）："
      printf '%s\n' "$missing" | sed 's/^/    - /'
      rm -f "$now"
      exit 1
    fi
    echo "✅ 基线的 $(wc -l < "$base" | tr -d ' ') 条用例全部还在（当前 $n 条）"
    rm -f "$now"
    ;;
  *)
    echo "用法: $0 {snapshot <out>|compare <base>}" >&2
    exit 2
    ;;
esac
