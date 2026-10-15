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
hits=$(grep -rnE 'Dockerfile\.agent|agentfleet-agent|agent-binaries|docker-agent|agent 镜像' --include='*.md' docs/ .mdflow/ 2>/dev/null || true)
if [[ -z "$hits" ]]; then ok "命名合规"; else bad "出现把设备二进制叫 agent 的用法："; echo "$hits" | sed 's/^/      /'; fi

echo "== C3 里程碑文档的 docs/architecture/*.md:N 锚点必须指向小节标题 =="
if [[ -f .mdflow/milestones/M0-device-enrollment.md ]]; then
  python3 - <<'PYEOF'
import re, os, sys
p='.mdflow/milestones/M0-device-enrollment.md'
s=open(p,encoding='utf-8').read()
refs=re.findall(r'`?(docs/[A-Za-z0-9_\-/\.]+\.md):(\d+)`?', s)
bad=[]
for f,n in refs:
    n=int(n)
    if not os.path.exists(f): bad.append((f,n,'FILE MISSING')); continue
    lines=open(f,encoding='utf-8').read().split('\n')
    if n<1 or n>len(lines) or not lines[n-1].startswith('#'):
        bad.append((f,n,'NOT A HEADING'))
if bad:
    for f,n,why in bad: print(f"  ❌ {f}:{n} {why}")
    sys.exit(1)
print(f"  ✅ {len(refs)} 个锚点全部有效")
PYEOF
  [[ $? -ne 0 ]] && fail=1
else
  echo "  ⏭️  无里程碑文档，跳过"
fi

echo
if [[ $fail -eq 0 ]]; then echo "门禁通过"; else echo "门禁未通过"; exit 1; fi
