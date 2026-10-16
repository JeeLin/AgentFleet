#!/usr/bin/env bash
# 等磁盘腾出窗口再跑覆盖率门禁。
#
# 存在的原因：llvm-cov 需要约 6G 空闲，而同机的另一个项目也在做插桩构建，
# 两者会互相把盘写满（表现为链接器 Bus error，看起来像代码问题）。
# 这个脚本**只等待，不删别人的东西**——清别的项目的 target 会打断它正在跑的构建。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG="${COV_LOG:-/tmp/cov_auto.log}"
NEEDED_KB=6600000   # 比脚本自带的 6144MiB 多留一点余量
TRIES="${COV_TRIES:-40}"

: > "$LOG"
for i in $(seq 1 "$TRIES"); do
    avail_kb=$(df -Pk "$ROOT" | awk 'NR==2 {print $4}')
    if [ "$avail_kb" -ge "$NEEDED_KB" ]; then
        echo "==> attempt $i: $((avail_kb / 1024))MiB free, running the gate" | tee -a "$LOG"
        cd "$ROOT" || exit 1
        scripts/check-coverage.sh >> "$LOG" 2>&1
        code=$?
        echo "==> coverage exit=$code" >> "$LOG"
        exit "$code"
    fi
    echo "==> attempt $i: only $((avail_kb / 1024))MiB free, waiting" >> "$LOG"
    sleep 60
done

echo "==> TIMEOUT: never reached ${NEEDED_KB}KiB in ${TRIES} tries" >> "$LOG"
exit 1
