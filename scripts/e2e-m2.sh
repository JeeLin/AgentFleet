#!/usr/bin/env bash
# M2 真设备端到端验收（里程碑《M2-acp-conversation》子任务 13）。
#
# ## 它到底做了什么
#
# 起**真** afhub、起**真** afcli（`afcli run`，supervisor + worker 两个进程），
# 再起**真** opencode（由设备侧的 ACP 连接器亲自 spawn，脚本不直接碰它），
# 然后按页面走的那条路逐条断言 M2 的四个基本要求：
#
#   页面 → Hub：`POST /api/*`（session cookie + `X-CSRF-Token`）
#   Hub → 页面：`GET /api/stream`（SSE，`channel.frame` / `channel.ready`）
#   Hub ↔ 设备：`/ws`（协议帧，由真 afcli 自己维持）
#   设备 → agent：ACP over stdio（`opencode acp`，由 `node-core` 的 `AcpClient` 起）
#
# 所以这条链路上**没有一处是替身**：写配置走 `POST /api/agents/config`，
# 发消息走 `POST /api/sessions/{id}/conversation` + 应答请求，读回复走 SSE 的
# `channel.frame`，读轨迹走 `GET /api/sessions/{id}/trajectory`。
#
# ## 断言分级
#
# 自动断言：1、2、3、4、5、6、7、8 —— 失败即非零退出。
# 其中 1 的后半、以及整条 4 都有「**跳过**」这一档（见 README §3）：
#   - 断言 1 的后半（移走 agent → 它消失）要动系统里的 opencode，
#     脚本只在 `--uninstall-drill` 下做；
#   - 断言 4 需要设备上真有一个**能对话**的 agent（凭据 / 模型可用）。
#     没有时脚本**明说跳过**，绝不把「没跑」画成 ✅。
#
# ## 用法
#
#   bash scripts/e2e-m2.sh                      # 全自动跑一遍，结束即清理
#   bash scripts/e2e-m2.sh --keep               # 保留日志现场（临时目录路径会打出来）
#   bash scripts/e2e-m2.sh --uninstall-drill    # 追加断言 1 的后半（会临时移走 PATH 里所有 opencode 入口）
#   bash scripts/e2e-m2.sh --port 19000         # 指定 Hub 端口
#
# 先决条件见 `scripts/e2e-m2/README.md`。**不改仓库里的 `data/`**：Hub 与设备的
# 数据目录都在本次运行的临时目录里（`AFHUB_DATA_DIR` / `AFCLI_DATA_DIR`）。
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

KEEP=0
UNINSTALL_DRILL=0
PORT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1 ;;
    --uninstall-drill) UNINSTALL_DRILL=1 ;;
    --port)
      PORT=${2:-}
      shift
      ;;
    -h | --help)
      awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
      exit 0
      ;;
    *)
      echo "未知参数：$1（--help 看用法）" >&2
      exit 2
      ;;
  esac
  shift
done

pass=0
fail=0
skip=0
ok() { echo "  ✅ $1"; pass=$((pass + 1)); }
bad() { echo "  ❌ $1"; fail=$((fail + 1)); }
# 「没跑」必须与「过了」在计数上分开。合并成一个计数就是这条里程碑最怕的那种
# 假绿：报告里一行 ✅ 覆盖了一段从未执行的断言。
skipped() { echo "  ⏭️  $1"; skip=$((skip + 1)); }
info() { echo "     $1"; }

# 夹具口令只此一处。改密码漏改一处 → setup 与 login 用的不是同一个 →
# 登录 401 → 拿不到 af_csrf → 脚本报「CSRF 实现坏了」，把口令问题说成实现问题。
PW='correct horse battery'

# ---------------------------------------------------------------------------
# 现场清理：无论正常结束、断言失败还是 Ctrl-C，都不留 afhub / afcli 僵尸
# ---------------------------------------------------------------------------

# 本次运行的设备进程（supervisor + worker）。**按 argv[0] 认，不按整条命令行认**：
# 早先用 `*"afcli"*` 匹配整条 cmdline，于是任何**命令行里碰巧含 afcli 字样**的进程
# 都会被收进来——`bash scripts/e2e-m2.sh`（脚本路径里没有，但编辑器 / grep / 本脚本
# 自己的子 shell 可能有）、`grep afcli`、`tail -f .../afcli.log` 全部中招。
# 症状是断言 8 拿一堆无关 pid 去比对，报出 afhub 在监听——那是个假红。
# 只认 argv[0] 的**文件名**是 afcli（绝对路径或裸名都行），才是在问「哪些是设备进程」。
stray_pids() {
  local p pid first base
  for p in /proc/[0-9]*; do
    pid=${p#/proc/}
    [ "$pid" = "$$" ] && continue
    first=$(tr '\0' '\n' 2>/dev/null <"$p/cmdline" | head -1) || continue
    [ -n "$first" ] || continue
    base=${first##*/}
    case "$base" in
      afcli) echo "$pid" ;;
    esac
  done
}

# `--uninstall-drill` 临时移走 opencode 的落点。清理时**必须先还原再删临时目录**：
# 还原失败而先把目录删了，等于把用户装的 opencode 弄丢（这台机器上它是唯一的
# 对话 agent，丢了脚本又报不出原因）。
#
# # 为什么要移走**多个**入口，不是 `command -v` 那一个
#
# 实测本机 `PATH` 里有两个 `opencode`：
#   - `…/mise/installs/npm-opencode-cli/latest/bin/opencode`（真二进制）
#   - `…/mise/shims/opencode`（mise 的 shim，`readlink -f` 解到 mise 本体）
#
# 早先只 `readlink -f "$(command -v opencode)"` 移走第一个。结果重扫时
# `path_lookup` 找到了第二份 shim——**它能被找到、但跑不起来**（目标二进制
# 不在了，shim 退出码 127），于是探测结果是 `Probe::Failed`「退出码 1」而**不是**
# `Probe::NotInstalled`。
#
# 那按契约是**对的**：「装了、但坏到跑不起来」必须带着原因列出来，好让用户看见
# 「你的 opencode 坏了」而不是让它从目录里消失。所以 red 的不是产品，是这条
# drill 没把「卸载」演干净——它演成了「装坏了」。
#
# 移走**入口本身**（不 `readlink -f` 解引用）：解引用会把 `mv` 落到 mise 的共享
# 本体上，一个入口对应多个 shim 时会互相覆盖。
OPENCODE_ENTRIES=()
OPENCODE_MOVED=()
restore_opencode() {
  local i
  for i in "${!OPENCODE_ENTRIES[@]}"; do
    local src="${OPENCODE_ENTRIES[$i]}"
    local moved="${OPENCODE_MOVED[$i]:-}"
    if [ -n "$moved" ] && [ -e "$moved" ] && [ ! -e "$src" ]; then
      mv "$moved" "$src" 2>/dev/null && echo "     已还原 $src"
    fi
  done
  # 还原之后必须真的能跑了，否则下次跑脚本会以一个「装坏了」的环境开局。
  if [ "${#OPENCODE_ENTRIES[@]}" -gt 0 ]; then
    if opencode --version >/dev/null 2>&1; then
      echo "     还原后 opencode --version 正常"
    else
      echo "     ⚠️ 还原后 opencode 仍然跑不起来，请人工检查：${OPENCODE_ENTRIES[*]}"
    fi
  fi
  OPENCODE_ENTRIES=()
  OPENCODE_MOVED=()
}

cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  restore_opencode
  [ -n "${SSE_PID:-}" ] && kill "$SSE_PID" 2>/dev/null
  # node 用 setsid 起，整组一起收：supervisor 走了 worker 不一定跟着走
  if [ -n "${NODE_PID:-}" ]; then
    kill -TERM "-$NODE_PID" 2>/dev/null || kill -TERM "$NODE_PID" 2>/dev/null
  fi
  [ -n "${HUB_PID:-}" ] && kill -TERM "$HUB_PID" 2>/dev/null
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ -z "$(stray_pids)" ] && break
    sleep 0.2
  done
  for pid in $(stray_pids); do kill -KILL "$pid" 2>/dev/null; done
  [ -n "${HUB_PID:-}" ] && kill -KILL "$HUB_PID" 2>/dev/null
  wait 2>/dev/null
  local left
  left=$(stray_pids | tr '\n' ' ')
  if [ -n "$left" ]; then
    echo "  ❌ 清理失败，仍有设备进程残留（pid: $left）"
    rc=1
  fi
  if [ "$KEEP" -eq 1 ]; then
    echo "  现场保留在：$TMP"
  elif [ -n "${TMP:-}" ]; then
    rm -rf "$TMP"
  fi
  exit "$rc"
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# 0. 先决条件
# ---------------------------------------------------------------------------
echo "== 0 先决条件 =="
missing=""
for tool in curl jq ss sqlite3 awk sed timeout; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
  echo "  ❌ 缺工具：$missing（断言 8 要 ss，断言 5 要 sqlite3）" >&2
  exit 2
fi
echo "  ✅ 工具齐备（curl / jq / ss / sqlite3 / timeout）"

AFHUB_BIN=${AFHUB_BIN:-$ROOT/target/debug/afhub}
AFCLI_BIN=${AFCLI_BIN:-$ROOT/target/debug/afcli}
if [ ! -x "$AFHUB_BIN" ] || [ ! -x "$AFCLI_BIN" ]; then
  echo "  ⏳ 二进制不存在，先构建（CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1}）"
  CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1} cargo build --bin afhub --bin afcli || {
    echo "  ❌ 构建失败：验收无法开始" >&2
    exit 2
  }
fi
if [ ! -x "$AFHUB_BIN" ] || [ ! -x "$AFCLI_BIN" ]; then
  echo "  ❌ 找不到 $AFHUB_BIN / $AFCLI_BIN（用 AFHUB_BIN / AFCLI_BIN 指定）" >&2
  exit 2
fi
echo "  ✅ 验收对象：$AFHUB_BIN / $AFCLI_BIN"
info "本机：$(uname -sm)，内核 $(uname -r)，cores=$(nproc 2>/dev/null || echo '?')"

# ---------------------------------------------------------------------------
# 设备上得真有一个能对话的 agent。**期望值取自 `opencode --version` 的真实输出**，
# 不写死：写死的话断言 1 变成「上报值 == 我抄的值」，装了新版 opencode 仍然是绿的，
# 而它到底有没有被探测出来这件事一次都没验过（M1 标准 10 的教训）。
# ---------------------------------------------------------------------------
OPENCODE_BIN=$(command -v opencode 2>/dev/null || true)
if [ -z "$OPENCODE_BIN" ]; then
  echo "  ❌ PATH 里没有 opencode：断言 1/3/4 都需要设备上真有一个 agent" >&2
  echo "     （怎么装见 scripts/e2e-m2/README.md §3）" >&2
  exit 2
fi
# 期望值就是 `opencode --version` 第一行的**原文**，不剥前缀、不改格式：
# 设备侧 `run_version()` 存进 `agent_runtime.version` 的也是那一行的原文
# （实测 `opencode v2.1.4`）。两边都不加工地比，才是「上报值 == 真实输出」；
# 我若在这边剥成 `2.1.4`、那边存的是原文，就会拿两个我改过的东西互相证明。
OPENCODE_RAW=$("$OPENCODE_BIN" --version 2>&1 | head -1)
EXPECT_VERSION=$(printf '%s' "$OPENCODE_RAW" | sed -e 's/[[:space:]]*$//')
if [ -z "$EXPECT_VERSION" ]; then
  echo "  ❌ 读不出 opencode 版本：'$OPENCODE_RAW'" >&2
  exit 2
fi
echo "  ✅ 设备 agent：$OPENCODE_BIN（\`--version\` 原文 '$EXPECT_VERSION'，断言 1 拿它比）"

if ! "$OPENCODE_BIN" acp --help >/dev/null 2>&1; then
  echo "  ❌ 这个 opencode 没有 acp 子命令：断言 4 无法执行（不是脚本的问题）" >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# 现场：临时目录 + 工作区夹具 + 端口
# ---------------------------------------------------------------------------
TMP_BASE=${TMPDIR:-/tmp}
mkdir -p "$TMP_BASE" 2>/dev/null || TMP_BASE=/tmp
TMP=$(mktemp -d "$TMP_BASE/agentfleet-e2e-m2.XXXXXX") || {
  echo "  ❌ 无法创建临时目录（TMPDIR=$TMP_BASE）" >&2
  exit 2
}
mkdir -p "$TMP/hub" "$TMP/node" "$TMP/ws/src"
WS=$TMP/ws
printf 'hello from the m2 e2e workspace\n' >"$WS/a.txt"

HUB_LOG=$TMP/hub.log
SSE_LOG=$TMP/sse.log
COOKIES=$TMP/cookies.txt
BODY=$TMP/last-body.json
NODE_LOG_DIR=$TMP/node/agentfleet
# Hub 的库文件。断言 1 要读 `agent_runtime` 表取版本号（`/api/agents` 不暴露它），
# 路径在这里定一处——散在断言里各拼一次，改目录名时会漏改一处而静默读空。
HUB_DB=$TMP/hub/agentfleet.db

if [ -z "$PORT" ]; then
  for _ in $(seq 1 20); do
    PORT=$(((RANDOM % 2000) + 18000))
    ss -ltn 2>/dev/null | grep -q ":$PORT " || break
  done
fi
BASE="http://127.0.0.1:$PORT"
echo "  现场：$TMP（Hub 端口 $PORT，工作区 $WS）"

# ---------------------------------------------------------------------------
# 起 Hub（真 afhub）
# ---------------------------------------------------------------------------
echo "== 起真 afhub =="
AFHUB_DATA_DIR=$TMP/hub AFHUB_ADDR="127.0.0.1:$PORT" "$AFHUB_BIN" >>"$HUB_LOG" 2>&1 &
HUB_PID=$!
for _ in $(seq 1 100); do
  curl -sS -o /dev/null "$BASE/healthz" 2>/dev/null && break
  kill -0 "$HUB_PID" 2>/dev/null || {
    echo "  ❌ afhub 启动即退出，日志：" >&2
    tail -20 "$HUB_LOG" >&2
    exit 1
  }
  sleep 0.1
done
if ! curl -sS -o /dev/null "$BASE/healthz" 2>/dev/null; then
  echo "  ❌ afhub 10s 内没有响应 /healthz" >&2
  tail -20 "$HUB_LOG" >&2
  exit 1
fi
ok "afhub 在 $BASE 上响应"

# 首次 setup：token 从日志里取（Hub 会把它打出来）。取不到就是 Hub 的行为变了，
# 直接退出而不是退回一个猜的值。
SETUP_TOKEN=$(sed 's/\x1b\[[0-9;]*m//g' "$HUB_LOG" | sed -n 's/.*setup token.*: *\([A-Za-z0-9_-]*\).*/\1/p' | head -1)
if [ -z "$SETUP_TOKEN" ]; then
  echo "  ❌ 日志里找不到首次 setup token（Hub 的文案变了？）" >&2
  tail -20 "$HUB_LOG" >&2
  exit 1
fi
SETUP_CODE=$(curl -sS -o "$BODY" -w '%{http_code}' -X POST "$BASE/setup" \
  -H "x-setup-token: $SETUP_TOKEN" -H 'content-type: application/json' \
  --data-binary "{\"password\":\"$PW\"}")
# 成功是 **200 + `{"initialized":true}`**（不是 201：这个端点不创建资源，
# 它给已有的那台 Hub 设一个密码）。判据用响应体一起卡，别只看状态码——
# 一个只回 200 却什么都没设的实现会让后面的 login 404，而错因显示在这里最好查。
SETUP_BODY=$(cat "$BODY" 2>/dev/null)
if [ "$SETUP_CODE" != "200" ] || ! printf '%s' "$SETUP_BODY" | jq -e '.initialized == true' >/dev/null 2>&1; then
  echo "  ❌ setup 回 $SETUP_CODE / $SETUP_BODY（期望 200 + initialized:true）" >&2
  exit 1
fi
LOGIN_CODE=$(curl -sS -c "$COOKIES" -o "$BODY" -w '%{http_code}' -X POST "$BASE/login" \
  -H 'content-type: application/json' --data-binary "{\"password\":\"$PW\"}")
if [ "$LOGIN_CODE" != "200" ]; then
  echo "  ❌ login 回 $LOGIN_CODE（期望 200）：$(cat "$BODY")" >&2
  exit 1
fi
CSRF=$(awk '$6 == "af_csrf" { print $7 }' "$COOKIES" | tail -1)
if [ -z "$CSRF" ]; then
  echo "  ❌ cookie jar 里没有 af_csrf：后续每个 POST 都会 403" >&2
  exit 1
fi
ok "setup + login，拿到 af_csrf"

# ---------------------------------------------------------------------------
# 认证口径只此一处：cookie jar 与 CSRF 头名都从这里出。
#
# 散在多处各写一遍时，改 CSRF 头名或 cookie 名只改得了一处 → 另一半静默失效
# （写侧 403，读侧拿着旧 cookie 静默拿回未登录响应），现象是「部分接口通、个别不通」，
# 排障时看不出是同一处配置漂移。
# ---------------------------------------------------------------------------
curl_auth() {
  curl -sS -b "$COOKIES" -H "x-csrf-token: $CSRF" "$@"
}

# SSE 订阅（浏览器 EventSource 的那条）
: >"$SSE_LOG"
curl_auth -N "$BASE/api/stream" >>"$SSE_LOG" 2>&1 &
SSE_PID=$!

# ---------------------------------------------------------------------------
# `METHOD PATH [BODY]` → HTTP_CODE / LAST_BODY 两个全局量
#
# **不打印 stdout**：结果只经全局量读。注意别在 `$(...)` 里读 LAST_BODY——
# 赋值发生在子 shell 里，父 shell 的 LAST_BODY 留着的是上一次的残留。
# ---------------------------------------------------------------------------
api() {
  local method=$1 path=$2 data=${3-}
  if [ -n "$data" ]; then
    HTTP_CODE=$(curl_auth -o "$BODY" -w '%{http_code}' -X "$method" \
      -H 'content-type: application/json' "$BASE$path" --data-binary "$data")
  else
    HTTP_CODE=$(curl_auth -o "$BODY" -w '%{http_code}' -X "$method" "$BASE$path")
  fi
  LAST_BODY=$(cat "$BODY" 2>/dev/null)
  return 0
}

# SSE 里某个事件名的所有 `data:`（一行一条）：`<id><TAB><json>`
sse_lines() {
  awk -v want="$1" '
    /^id: /    { id = substr($0, 4); gsub(/^ +/, "", id) }
    /^event: / { ev = substr($0, 7); gsub(/^ +/, "", ev) }
    /^data: /  { if (ev == want) { d = substr($0, 6); sub(/^ /, "", d); printf "%s\t%s\n", id, d } }
  ' "$SSE_LOG"
}

# 某条通道的所有 `channel.frame` 载荷（JSON 行）
frames_of() {
  sse_lines channel.frame | cut -f2- | jq -c --arg c "$1" 'select(.channel_id == $c) | .payload' 2>/dev/null
}

wait_until() {
  local deadline=$((SECONDS + $1))
  shift
  while :; do
    if "$@"; then return 0; fi
    [ "$SECONDS" -ge "$deadline" ] && return 1
    sleep 0.3
  done
}

# 断言失败时把现场摊开：一条「断言失败」而没有现场，等于让人重新手工跑一遍。
dump() {
  local what=$1
  echo "     —— 现场（$what）——"
  echo "     Hub 日志里与通道/对话/错误有关的行（末 10 行）："
  sed 's/\x1b\[[0-9;]*m//g' "$HUB_LOG" | grep -E 'channel|conversation|trajectory|WARN|ERROR' | tail -10 | sed 's/^/       /'
  if [ -f "$NODE_LOG_DIR/logs/afcli.log" ]; then
    echo "     设备日志（末 10 行）："
    sed 's/\x1b\[[0-9;]*m//g' "$NODE_LOG_DIR/logs/afcli.log" | tail -10 | sed 's/^/       /'
  fi
  echo "     SSE：$(sse_lines channel.frame | wc -l) 条 channel.frame、$(sse_lines channel.ready | wc -l) 条 channel.ready"
}

# ---------------------------------------------------------------------------
# 起设备（真 afcli，一条命令）
# ---------------------------------------------------------------------------
echo "== 起真 afcli =="
NODE_CMD=(env "AFCLI_DATA_DIR=$TMP/node" "AFCLI_HUB_URL=ws://127.0.0.1:$PORT/ws" "$AFCLI_BIN" run)
setsid "${NODE_CMD[@]}" >>"$TMP/node.stdout" 2>&1 &
NODE_PID=$!

NODE_ID=""
DEVICE_STATUS=""
for _ in $(seq 1 200); do
  api GET /api/devices
  NODE_ID=$(printf '%s' "$LAST_BODY" | jq -r '.[0].id // empty' 2>/dev/null)
  DEVICE_STATUS=$(printf '%s' "$LAST_BODY" | jq -r '.[0].status // empty' 2>/dev/null)
  [ -n "$NODE_ID" ] && [ "$DEVICE_STATUS" = "online" ] && break
  sleep 0.2
done
if [ -z "$NODE_ID" ] || [ "$DEVICE_STATUS" != "online" ]; then
  echo "  ❌ 设备 40s 内没有上线（status='$DEVICE_STATUS'）" >&2
  dump "设备上线"
  exit 1
fi
ok "设备 $NODE_ID 已上线"

# ---------------------------------------------------------------------------
# 1（要求 1）设备扫描到 agent，心跳报上来的是**探测结果**而不是写死的清单
# ---------------------------------------------------------------------------
echo
echo "== 1（要求 1）心跳上报 agent 运行时 =="
# 探测在设备启动时做，心跳带的是那一刻的结果；刚上线时可能还在第一轮，
# 所以给一个窗口而不是一次性读。
agents_ready() {
  api GET /api/agents
  # 空输入下 jq 的退出码是 0，`length > 0` 会「通过」。用 `-r` 取值再比。
  [ "$(printf '%s' "$LAST_BODY" | jq -r 'length' 2>/dev/null)" -gt 0 ] 2>/dev/null
}
if ! wait_until 60 agents_ready; then
  bad "60s 内 /api/agents 一直为空——设备没上报任何 agent"
  dump "断言 1"
else
  api GET /api/agents
  AGENTS_DUMP=$(printf '%s' "$LAST_BODY" | jq -r '.[] | "\(.name)\t\(.available)"' 2>/dev/null)
  # **两个方向都验**：只验「有 opencode」证明不了它是探测出来的，
  # 也可能是一份写死的清单里正好也有它。
  if printf '%s' "$AGENTS_DUMP" | cut -f1 | grep -qx 'opencode'; then
    GOT_AVAILABLE=$(printf '%s' "$AGENTS_DUMP" | awk -F'\t' '$1 == "opencode" { print $2 }' | head -1)
    # **版本号从 `agent_runtime` 表读**，不从 `/api/agents` 读：目录项的对外契约
    # 只有 `{name, available, error}`（那是页面需要的三样），**没有 `version`**。
    # 早先这里按 `.[].version` 取，拿到的是空串，于是断言 1 永远红——而红的原因
    # 看起来像「探测没上报版本」，实际是「脚本读了一个契约里不存在的字段」。
    #
    # 期望值也**不是**剥过前缀的裸版本：`run_version()` 存的是 `--version` 第一行
    # 的**原文**（`opencode v2.1.4`），设备报的就是它。两边都不加工地比，才是
    # 「上报值 == 真实输出」；各加工一道就等于在比两个我都改过的东西。
    GOT_VERSION=$(sqlite3 "$HUB_DB" \
      "select version from agent_runtime where node_id='$NODE_ID' and agent_id='opencode';" 2>/dev/null)
    if [ "$GOT_VERSION" = "$EXPECT_VERSION" ]; then
      ok "opencode 上报版本 '$GOT_VERSION'，与 \`opencode --version\` 的原文一致（available=$GOT_AVAILABLE）"
    else
      bad "opencode 上报版本 '$GOT_VERSION' ≠ \`opencode --version\` 的原文 '$EXPECT_VERSION'"
      dump "断言 1"
    fi
    if [ "$GOT_AVAILABLE" = "true" ]; then
      ok "opencode 报 available=true（Probe::Ready，ACP 入口可用）"
    else
      bad "opencode 装了却报 available=$GOT_AVAILABLE：断言 3/4 都会连带失败"
      info "  该条的 error 字段：$(printf '%s' "$LAST_BODY" | jq -r '.[] | select(.name=="opencode") | .error // "(无)"')"
      dump "断言 1"
    fi
  else
    bad "设备上装了 opencode，但 /api/agents 里没有它——探测没跑通"
    info "  实际上报：$(printf '%s' "$AGENTS_DUMP" | tr '\n' ' ')"
    dump "断言 1"
  fi
fi

# 1 的后半：移走 → 它消失。这一步要动系统里的 opencode，默认不做。
if [ "$UNINSTALL_DRILL" -eq 1 ]; then
  echo "  -- 卸载探测（--uninstall-drill）：移走 opencode --"
  # 枚举 `PATH` 里**所有**名为 opencode 的可执行入口（含 mise shim），
  # 理由见 `restore_opencode` 上方那段注释：只移 `command -v` 那一个会演成
  # 「装坏了」（剩下一个指向不存在目标的 shim → `Probe::Failed`）而不是
  # 「卸掉了」（`Probe::NotInstalled`）。两者按契约该有不同表现，别混淆。
  for _dir in ${PATH//:/ }; do
    # `-x` 就够：它能穿过符号链接判断最终目标可执行，正是 `path_lookup`
    # 的判据（`is_executable`）。这里要和产品同口径，否则 drill 用的清单
    # 与设备扫到的清单可能不是同一份。
    if [ -x "$_dir/opencode" ]; then
      OPENCODE_ENTRIES+=("$_dir/opencode")
    fi
  done
  if [ "${#OPENCODE_ENTRIES[@]}" -eq 0 ]; then
    bad "PATH 里找不到 opencode，--uninstall-drill 无从演起"
  else
    echo "     要移走的入口：${OPENCODE_ENTRIES[*]}"
    _moved_all=1
    for _i in "${!OPENCODE_ENTRIES[@]}"; do
      _src="${OPENCODE_ENTRIES[$_i]}"
      _dst="$_src.e2e-moved.$$"
      if mv "$_src" "$_dst" 2>/dev/null; then
        OPENCODE_MOVED[$_i]="$_dst"
      else
        bad "移不动 $_src：--uninstall-drill 需要能改它所在的目录"
        _moved_all=0
        break
      fi
    done
    if [ "$_moved_all" -eq 1 ]; then
      # 先自证「真的卸干净了」：这时候 shell 里也不该再找得到 opencode。
      # 没有这一句，drill 演成「装坏了」时整条链路仍然全绿，而那条绿是假的。
      if command -v opencode >/dev/null 2>&1; then
        bad "移走之后 shell 里还能找到 opencode——drill 没演干净，后面的判据不作数"
        info "  还找得到：$(command -v opencode)"
      else
        ok "移走后 shell 里已经找不到 opencode（drill 演的是「卸载」不是「装坏了」）"
      fi
      # 重启设备进程：探测发生在启动时，改 PATH 不会让已跑的进程重扫。
      kill -TERM "-$NODE_PID" 2>/dev/null || kill -TERM "$NODE_PID" 2>/dev/null
      wait "$NODE_PID" 2>/dev/null
      setsid "${NODE_CMD[@]}" >>"$TMP/node.stdout" 2>&1 &
      NODE_PID=$!
      opencode_gone() {
        api GET /api/agents
        ! printf '%s' "$LAST_BODY" | jq -r '.[].name' 2>/dev/null | grep -qx 'opencode'
      }
      if wait_until 90 opencode_gone; then
        ok "移走 opencode 后设备重扫，它从 /api/agents 里消失（证明是探测出来的）"
      else
        bad "opencode 已被移走，90s 后 /api/agents 里还有它——那是缓存不是探测"
        dump "断言 1 卸载"
      fi
      restore_opencode
      # **还原之后也要重启设备**：探测只在启动时做一次（注释在卸载那一段）。
      # 少了这一步，「消失了」过了、「回来了」永远过——而那条红是假的：设备
      # 只是没再看一眼，不是探测路径有状态残留。实测（子任务 13）：不重启时
      # `agent_runtime` 里确实只剩 claude，opencode 的消失是对的，回来是没扫。
      kill -TERM "-$NODE_PID" 2>/dev/null || kill -TERM "$NODE_PID" 2>/dev/null
      wait "$NODE_PID" 2>/dev/null
      setsid "${NODE_CMD[@]}" >>"$TMP/node.stdout" 2>&1 &
      NODE_PID=$!
      # 还原之后必须能重新探测到，否则「消失」可能只是设备没起来。
      opencode_back() {
        api GET /api/agents
        printf '%s' "$LAST_BODY" | jq -r '.[].name' 2>/dev/null | grep -qx 'opencode'
      }
      if wait_until 90 opencode_back; then
        ok "还原 opencode 后它重新出现在 /api/agents 里"
      else
        bad "还原 opencode 后 90s 内没回来——探测路径有状态残留"
        dump "断言 1 还原"
      fi
    else
      restore_opencode
    fi
  fi
else
  skipped "断言 1 后半（移走 opencode → 它消失）：加 --uninstall-drill 才跑"
fi

# ---------------------------------------------------------------------------
# 2（要求 2）agent 配置：写进去 / 读回来一致 / 删掉回默认
#
# **这一段必须在最后把自己的配置删掉**，否则它会成为断言 3–6 的实际启动方式。
# 这不是理论风险：第一版脚本留着 `args=["--model",…]` 没收尾，于是设备真的执行了
# `opencode acp --model …`——而那正是我们在 4b 里测到的「CLI 解析阶段就退」。
# 表现是断言 4 报「60s 内没有 ready」，看起来完全像「对话实现坏了 / 环境不行」，
# 而真凶是**测试自己写的配置**。这条教训已写进 README §4。
#
# 「配置真被设备用上」不在这一段验，放在 4b、用一条**从没跑过对话**的会话验：
# 那样它才不会污染断言 4/5/6 的「正常路径」。
# ---------------------------------------------------------------------------
echo
echo "== 2（要求 2）agent 启动配置：写读往返 =="
# `args` 这里放 `--model`：它是**用户配置文件里合法**的参数，用来验「原样存取」。
CFG_ARGS='["--model","definitely-not-a-real-model/e2e-marker"]'
CFG_ENV='{"E2E_MARKER":"m2"}'
api POST /api/agents/config "{\"node_id\":\"$NODE_ID\",\"agent_id\":\"opencode\",\"command\":\"\",\"args\":$CFG_ARGS,\"env\":$CFG_ENV}"
if [ "$HTTP_CODE" = "200" ] || [ "$HTTP_CODE" = "201" ]; then
  ok "写配置回 $HTTP_CODE"
else
  bad "写配置回 $HTTP_CODE（期望 2xx）：$LAST_BODY"
  dump "断言 2 写"
fi
api GET "/api/agents/config?node_id=$NODE_ID"
READBACK=$(printf '%s' "$LAST_BODY" | jq -c '.configs[] | select(.agent_id == "opencode")' 2>/dev/null)
if [ -n "$READBACK" ]; then
  GOT_ARGS=$(printf '%s' "$READBACK" | jq -c '.args')
  GOT_ENV=$(printf '%s' "$READBACK" | jq -c '.env')
  if [ "$GOT_ARGS" = "$CFG_ARGS" ] && [ "$GOT_ENV" = "$CFG_ENV" ]; then
    ok "读回来一致：args=$GOT_ARGS env=$GOT_ENV"
  else
    bad "读回来的与写入的不一致：args=$GOT_ARGS（期望 $CFG_ARGS）env=$GOT_ENV（期望 $CFG_ENV）"
    dump "断言 2 读"
  fi
else
  bad "读不到刚写的配置：$LAST_BODY"
  dump "断言 2 读"
fi

# 删掉 → 回到默认启动方式。**一条判据**：读回来为空。
# （「删完之后新会话能起来」由断言 3 自己证明——它就跑在删除之后。）
DEL_CODE=$(curl_auth -o /dev/null -w '%{http_code}' -X DELETE \
  "$BASE/api/agents/config?node_id=$NODE_ID&agent_id=opencode" 2>/dev/null)
if [ "$DEL_CODE" = "204" ]; then
  ok "删配置回 204"
else
  bad "删配置回 $DEL_CODE（期望 204）：后续断言会被这条配置污染"
fi
api GET "/api/agents/config?node_id=$NODE_ID"
# 空输入下 jq 的退出码是 0，所以用 `-r` 取值再比，不用 `-e`。
LEFT=$(printf '%s' "$LAST_BODY" | jq -r '[.configs[] | select(.agent_id == "opencode")] | length' 2>/dev/null)
if [ "${LEFT:-1}" -eq 0 ]; then
  ok "读回来已经没有 opencode 的配置了（断言 3–6 用的是默认启动方式）"
else
  bad "删完还能读到 $LEFT 条 opencode 配置——断言 3–6 会拿它当启动方式"
fi

# ---------------------------------------------------------------------------
# 3（要求 3）建会话：选已装 agent → 201；选未装 → 409 且信息可读
# ---------------------------------------------------------------------------
echo
echo "== 3（要求 3）建会话能选 agent =="
SID=""
api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS\",\"agent_id\":\"opencode\"}"
if [ "$HTTP_CODE" = "201" ]; then
  SID=$(printf '%s' "$LAST_BODY" | jq -r '.id // empty')
  SID_AGENT=$(printf '%s' "$LAST_BODY" | jq -r '.agent_id // empty')
  if [ -n "$SID" ] && [ "$SID_AGENT" = "opencode" ]; then
    ok "选 opencode 建会话 201，session=$SID，agent_id=$SID_AGENT"
  else
    bad "建会话回 201 但内容不对（id='$SID' agent_id='$SID_AGENT'）"
    SID=""
    dump "断言 3"
  fi
else
  bad "选已装 agent 建会话回 $HTTP_CODE（期望 201）：$LAST_BODY"
  dump "断言 3"
fi

# 选一个设备上不存在的 agent → 409 + 可读的 error code
api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS\",\"agent_id\":\"definitely-not-installed-e2e\"}"
if [ "$HTTP_CODE" = "409" ]; then
  ERR_CODE=$(printf '%s' "$LAST_BODY" | jq -r '.error // empty')
  if [ -n "$ERR_CODE" ]; then
    ok "选未装 agent 回 409，error='$ERR_CODE'（页面能把它映射成一句人话）"
  else
    bad "409 但响应体里没有 error 字段：$LAST_BODY"
  fi
else
  bad "选未装 agent 回 $HTTP_CODE（期望 409）：$LAST_BODY"
fi

# ---------------------------------------------------------------------------
# 4（要求 4）发一条消息 → 收到 assistant 流式回复
#
# **判断顺序不能反**（README §4）：环境失败表现为 `session/new` 就回 JSON-RPC error，
# 不是 `stopReason`。所以先看通道有没有拿到 `ready`（= session/new 拿到了 sessionId），
# 再看 `turn_end` 的 stopReason。
# ---------------------------------------------------------------------------
echo
echo "== 4（要求 4）对话：发消息 → 收到流式回复 =="
TCH=""
CONV_READY=0
ACP_SID1=""
if [ -n "$SID" ]; then
  api POST "/api/sessions/$SID/conversation"
  if [ "$HTTP_CODE" = "201" ]; then
    TCH=$(printf '%s' "$LAST_BODY" | jq -r '.channel_id // empty')
    ok "开对话通道 201，channel=$TCH"
  else
    bad "开对话回 $HTTP_CODE（期望 201）：$LAST_BODY"
    dump "断言 4 开通道"
  fi
else
  bad "断言 3 没拿到会话号，断言 4 无法开始"
fi

# ACP 子进程要 spawn + initialize + session/new。实测首 token 波动 2.93–14.53 s，
# 这里给 60 s：**不能把「慢」判成「失败」**。
if [ -n "$TCH" ]; then
  conv_ready() {
    # `jq -e 'select(...)'` **在空输入下退出码是 0**（实测），挡不住「一帧都没来」。
    # 判据必须显式数匹配条数：> 0 才算条件成立。
    [ "$(frames_of "$TCH" | jq -r 'select(.type == "ready") | 1' 2>/dev/null | grep -c .)" -gt 0 ]
  }
  if wait_until 60 conv_ready; then
    CONV_READY=1
    ACP_SID1=$(frames_of "$TCH" | jq -r 'select(.type == "ready") | .sessionId // empty' | head -1)
    ok "agent 报会话 ready（ACP sessionId=$ACP_SID1）"
  else
    # 走到这里说明 session/new 没成功。按 README §4 的判据，这是**环境失败**的形态，
    # 不是「对话实现坏了」——但结论只能靠 agent 自己的错误说明。
    bad "60s 内没有 ready：session/new 没拿到 sessionId（环境失败，不是 prompt 失败）"
    dump "断言 4 ready"
  fi
fi

if [ "$CONV_READY" -eq 1 ]; then
  PROMPT_TEXT='请只回复两个字：好的'
  # 上行走**既有的裸字节通道** `POST /api/channels/{id}/send`（页面键盘输入用的
  # 就是它），载荷是 `{"text": ...}`：设备侧 `worker.rs::drain_inbound` 只从
  # `text` 字段取字符串。**没有专用的对话上行端点**——对话只借了开通道那一步。
  SEND_CODE=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    "$BASE/api/channels/$TCH/send" --data-binary "$(jq -cn --arg t "$PROMPT_TEXT" '{text:$t}')")
  if [ "$SEND_CODE" = "200" ] || [ "$SEND_CODE" = "202" ] || [ "$SEND_CODE" = "204" ]; then
    info "prompt 已下发（$SEND_CODE）"
  else
    bad "下发 prompt 回 $SEND_CODE：$(cat "$BODY")"
    dump "断言 4 prompt"
  fi

  # 首 token 到 turn_end 可能比 ready 还久（长回复实测首 chunk 8.29 s）。
  turn_done() {
    [ "$(frames_of "$TCH" | jq -r 'select(.type == "turn_end") | 1' 2>/dev/null | grep -c .)" -gt 0 ]
  }
  if wait_until 180 turn_done; then
    STOP=$(frames_of "$TCH" | jq -r 'select(.type == "turn_end") | .stopReason // empty' | head -1)
    # 拼出 assistant 的可见文本：ACP 的 `agent_message_chunk` 在 batch 的 events 里。
    # `content` 实测有两种形态：数组（`session/load` 重放）与**单个对象**
    # `{"type":"text","text":…}`（opencode acp 的实时 chunk）。只认数组会把
    # 实时那一轮的空串当成「一个 chunk 都没拼出来」。
    TOKENS_TEXT=$(frames_of "$TCH" \
      | jq -r 'select(.type == "batch") | .events[]? | select((.sessionUpdate // "") == "agent_message_chunk")
               | (if (.content | type) == "array" then ([.content[] | .text // ""] | join("")) else (.content.text // "") end)' 2>/dev/null \
      | tr -d '\n')
    if [ "$STOP" = "end_turn" ]; then
      ok "一轮结束 stopReason=end_turn"
    else
      bad "turn_end 的 stopReason='$STOP'（期望 end_turn）"
      dump "断言 4 turn_end"
    fi
    if [ -n "$TOKENS_TEXT" ]; then
      ok "收到 assistant 流式文本（${#TOKENS_TEXT} 字符）：${TOKENS_TEXT:0:40}"
    else
      bad "turn 结束了但一个 agent_message_chunk 都没拼出来"
      info "  通道帧类型分布：$(frames_of "$TCH" | jq -r '.type // "?"' 2>/dev/null | sort | uniq -c | tr '\n' ' ')"
      dump "断言 4 文本"
    fi
  else
    bad "180s 内没有 turn_end：prompt 没有走完（模型不可用？见 README §4）"
    dump "断言 4 turn_end"
  fi
fi

# ---------------------------------------------------------------------------
# 5 轨迹落库：user 与 assistant 各至少一条，五个硬要求可判非空
# ---------------------------------------------------------------------------
echo
echo "== 5 轨迹落库 =="
if [ -n "$SID" ] && [ "$CONV_READY" -eq 1 ]; then
  api GET "/api/sessions/$SID/trajectory"
  if [ "$HTTP_CODE" = "200" ]; then
    EV_COUNT=$(printf '%s' "$LAST_BODY" | jq -r '.events | length' 2>/dev/null)
    ASST_N=$(printf '%s' "$LAST_BODY" | jq -r '[.events[] | select(.kind == "agent_message_chunk")] | length' 2>/dev/null)
    if [ "${EV_COUNT:-0}" -gt 0 ]; then
      ok "轨迹里有 $EV_COUNT 条事件"
    else
      bad "轨迹为空：对话跑过了却没有一条落库"
    fi
    # **这里不断言 `user_message_chunk`。** 实测 opencode v2.1.4 的**实时**一轮
    # 只推 `available_commands_update` / `agent_message_chunk` / `usage_update`；
    # `user_message_chunk` **只在 `session/load` 的重放里出现**（断言 6 才走到）。
    #
    # 早先这里要求「轨迹里必须有 user_message_chunk」，于是断言 5 永远红——
    # 而红的原因是那条断言拿**重放数据**当实时数据的期望，不是产品缺东西。
    # 用户消息本身的真相源是 `POST /api/channels/{id}/send` 的请求体（页面
    # 手里就有），轨迹不重复记它。真要有那么一天 opencode 开始回显，
    # 它也会作为一条普通事件被原样存下（未知/新类型不丢），不会因此出错。
    [ "${ASST_N:-0}" -ge 1 ] && ok "assistant 事件 $ASST_N 条" || bad "轨迹里没有 agent_message_chunk"
    # 五个硬要求：id / seq / kind / payload / created_at 必须非空。
    # **`duration_ms` 按 `duration_source` 判**：实测 ACP 事件不带时间戳，要求它非空
    # 等于逼实现者填 0——那是一条撒谎的事实。
    #
    # **要的是「有几条缺」这个数字**，所以必须 `| length`。早先少了它，`MISSING`
    # 变成一串事件 JSON，`[ "$MISSING" -eq 0 ]` 因为不是数字而报错、落到 else 分支，
    # 打出「 条事件缺硬要求字段」（数字位置是空的）——断言红了却什么也没说。
    MISSING=$(printf '%s' "$LAST_BODY" | jq -r '
      [.events[]
       | select((.id == null) or (.seq == null) or ((.kind // "") == "")
                or ((.payload // "") == "") or ((.created_at // "") == ""))] | length' 2>/dev/null)
    if [ "${MISSING:-1}" -eq 0 ]; then
      ok "五个硬要求（id/seq/kind/payload/created_at）全部可判非空"
    else
      bad "$MISSING 条事件缺硬要求字段"
    fi
    # `duration_source` 只能是 measured / none；`none` 时 `duration_ms` 必须为 null
    # （填 0 会把「没测」说成「0 毫秒」）。
    BAD_DUR=$(printf '%s' "$LAST_BODY" | jq -r '
      [.events[]
       | select(((.duration_source != "measured") and (.duration_source != "none"))
                or ((.duration_source == "none") and (.duration_ms != null)))] | length')
    if [ "${BAD_DUR:-1}" -eq 0 ]; then
      ok "耗时列合规（duration_source ∈ {measured,none}，none 时 duration_ms 为 null）"
    else
      bad "$BAD_DUR 条事件的耗时列不合规（源与值矛盾）"
    fi
  else
    bad "读轨迹回 $HTTP_CODE：$LAST_BODY"
    dump "断言 5"
  fi
else
  bad "断言 4 没跑通，断言 5 无法判定"
fi

# ---------------------------------------------------------------------------
# 4b（要求 2 的后半）配置真的被设备用上了
#
# 只验「能在页面上读写」证明不了配置接进了启动路径：那样即使设备从来不读
# `agent_config`，断言 2 也全绿。
#
# 可观测的非法值选 **`command` 指向不存在的可执行文件**，不选 `args`：
# 实测 `opencode acp` **不接受 `--model`**（`Unrecognized flag`，CLI 解析阶段就退），
# 用它当非法值的话失败原因是「flag 不认识」而不是「配置生效了」，归因指错对象。
# `command` 指错路径的失败**必然带那个路径**，判据明确。
#
# **另建一条会话**：这条会话注定开不起对话，别让它污染断言 5/6 的「正常路径」。
# ---------------------------------------------------------------------------
echo
echo "== 4b（要求 2）配置真被设备用上 =="
BOGUS_BIN="$TMP/no-such-agent-binary-e2e"
api POST /api/agents/config "{\"node_id\":\"$NODE_ID\",\"agent_id\":\"opencode\",\"command\":\"$BOGUS_BIN\",\"args\":[],\"env\":{}}"
if [ "$HTTP_CODE" = "200" ] || [ "$HTTP_CODE" = "201" ]; then
  info "已把 opencode 的 command 指到不存在的 $BOGUS_BIN"
else
  bad "写非法配置回 $HTTP_CODE：$LAST_BODY"
fi

api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS\",\"agent_id\":\"opencode\"}"
SID_BAD=$(printf '%s' "$LAST_BODY" | jq -r '.id // empty' 2>/dev/null)
if [ "$HTTP_CODE" = "201" ] && [ -n "$SID_BAD" ]; then
  api POST "/api/sessions/$SID_BAD/conversation"
  if [ "$HTTP_CODE" = "201" ]; then
    TCH_BAD=$(printf '%s' "$LAST_BODY" | jq -r '.channel_id // empty')
    # 设备迟早会说「起不来」。正常时这条 ready 永远不来，所以只等一个短窗口：
    # 这一段验的是「失败会不会被说出来」，不是「多久说出来」。
    # 失败帧的判别字段是 **`op`**（值 `"error"`），不是 `type`；原因在 `reason`。
    # 早先这里按 `.type == "error"` 找，于是设备明明已经报了错、脚本却一直等满
    # 60 s 然后说「失败没有被说出来」——判据查错了字段，现象长得像产品 bug。
    bad_failed() {
      [ "$(frames_of "$TCH_BAD" \
        | jq -r 'select(.op == "error") | 1' 2>/dev/null \
        | grep -c .)" -gt 0 ]
    }
    bad_wrongly_ready() {
      [ "$(frames_of "$TCH_BAD" | jq -r 'select(.type == "ready") | 1' 2>/dev/null | grep -c .)" -gt 0 ]
    }
    if wait_until 60 bad_failed; then
      ERR_TEXT=$(frames_of "$TCH_BAD" | jq -r 'select(.op == "error") | (.reason // .message // "")' 2>/dev/null | head -1)
      # 归因判据：错误里必须出现那个不存在的路径。设备不读配置时，
      # 它会照常起来（opencode 走默认命令）→ ready 而不是失败。
      if printf '%s' "$ERR_TEXT" | grep -qF "$BOGUS_BIN"; then
        ok "对话失败且原因点名了那条配置（$BOGUS_BIN）——配置真被读进启动路径"
      else
        bad "对话失败了，但原因里没有 $BOGUS_BIN，归因不到那条配置：$ERR_TEXT"
        dump "断言 4b"
      fi
    elif bad_wrongly_ready; then
      bad "command 指向不存在的路径，对话却照样 ready——设备根本没读这份配置"
      dump "断言 4b"
    else
      bad "60s 内既没 ready 也没 error/exited：失败没有被说出来"
      dump "断言 4b"
    fi
    curl_auth -o /dev/null -X DELETE "$BASE/api/channels/$TCH_BAD" 2>/dev/null
  else
    bad "开对话回 $HTTP_CODE：$LAST_BODY"
  fi
else
  bad "用非法配置建会话回 $HTTP_CODE：$LAST_BODY"
fi

# 删掉 4b 的非法配置 → 回到默认启动方式。**判据是「读回来为空」**：
# 删完之后新会话能起来这件事由断言 3 之后的路径早已证明，不必再建一条。
# 这里**必须**删除：留着它会让后面的断言 6/7 里的对话全部起不来。
DEL_CODE=$(curl_auth -o /dev/null -w '%{http_code}' -X DELETE \
  "$BASE/api/agents/config?node_id=$NODE_ID&agent_id=opencode" 2>/dev/null)
if [ "$DEL_CODE" = "204" ]; then
  ok "删掉 4b 的非法配置回 204"
else
  bad "删掉 4b 的非法配置回 $DEL_CODE（期望 204）——后续断言会被它污染"
fi
api GET "/api/agents/config?node_id=$NODE_ID"
LEFT=$(printf '%s' "$LAST_BODY" | jq -r '[.configs[] | select(.agent_id == "opencode")] | length' 2>/dev/null)
if [ "${LEFT:-1}" -eq 0 ]; then
  ok "读回来已经没有 opencode 的配置了"
else
  bad "删完还能读到 $LEFT 条 opencode 配置"
fi

# ---------------------------------------------------------------------------
# 6 断开重连 → session/load 恢复同一 acp_session_id，且轨迹行数不变
# ---------------------------------------------------------------------------
echo
echo "== 6 重连用 session/load 恢复 =="
if [ -n "$SID" ] && [ "$CONV_READY" -eq 1 ] && [ -n "$ACP_SID1" ]; then
  # 关掉这条通道（模拟用户关页面），再开一次。
  curl_auth -o /dev/null -X DELETE "$BASE/api/channels/$TCH" 2>/dev/null
  sleep 1
  api GET "/api/sessions/$SID/trajectory"
  ROWS_BEFORE=$(printf '%s' "$LAST_BODY" | jq -r '.events | length' 2>/dev/null)
  api POST "/api/sessions/$SID/conversation"
  if [ "$HTTP_CODE" = "201" ]; then
    TCH2=$(printf '%s' "$LAST_BODY" | jq -r '.channel_id // empty')
    conv2_ready() {
      [ "$(frames_of "$TCH2" | jq -r 'select(.type == "ready") | 1' 2>/dev/null | grep -c .)" -gt 0 ]
    }
    if wait_until 60 conv2_ready; then
      ACP_SID2=$(frames_of "$TCH2" | jq -r 'select(.type == "ready") | .sessionId // empty' | head -1)
      if [ "$ACP_SID2" = "$ACP_SID1" ]; then
        ok "第二次开对话沿用了同一 ACP sessionId（$ACP_SID2）——走的是 load 不是 new"
      else
        bad "第二次开对话拿到新的 ACP sessionId（'$ACP_SID2' ≠ '$ACP_SID1'）——走的是 session/new，上下文已丢"
        dump "断言 6"
      fi
      api GET "/api/sessions/$SID/trajectory"
      ROWS_AFTER=$(printf '%s' "$LAST_BODY" | jq -r '.events | length' 2>/dev/null)
      if [ "${ROWS_BEFORE:-x}" = "${ROWS_AFTER:-y}" ]; then
        ok "轨迹行数在 load 前后不变（$ROWS_BEFORE → $ROWS_AFTER）：重放没有写库"
      else
        bad "轨迹行数从 $ROWS_BEFORE 变成 $ROWS_AFTER：重放事件被写进了轨迹"
      fi
      curl_auth -o /dev/null -X DELETE "$BASE/api/channels/$TCH2" 2>/dev/null
    else
      bad "第二次开对话 60s 内没有 ready（load 失败？判据看 ready 不看 turn_end）"
      dump "断言 6 重连"
    fi
  else
    bad "第二次开对话回 $HTTP_CODE：$LAST_BODY"
    dump "断言 6 重开"
  fi
else
  skipped "断言 6：需要断言 4 先拿到一个 ACP sessionId"
fi

# ---------------------------------------------------------------------------
# 7 心跳不因对话被堵
# ---------------------------------------------------------------------------
echo
echo "== 7 心跳不被对话堵住 =="
api GET /api/devices
LAST_SEEN_1=$(printf '%s' "$LAST_BODY" | jq -r --arg n "$NODE_ID" '.[] | select(.id == $n) | .last_seen // empty')
# 心跳间隔是 5 s 量级，离线判定是 45 s；等 20 s 足够看出「有没有被堵住」，
# 又不至于把正常的一两拍抖动读成故障。
sleep 20
api GET /api/devices
LAST_SEEN_2=$(printf '%s' "$LAST_BODY" | jq -r --arg n "$NODE_ID" '.[] | select(.id == $n) | .last_seen // empty')
STATUS_2=$(printf '%s' "$LAST_BODY" | jq -r --arg n "$NODE_ID" '.[] | select(.id == $n) | .status // empty')
if [ "$STATUS_2" = "online" ]; then
  ok "对话之后设备仍是 online（心跳没被堵）"
else
  bad "对话之后设备状态是 '$STATUS_2'——心跳被堵住了（M1 的 exec 教训）"
  dump "断言 7"
fi
if [ -n "$LAST_SEEN_2" ] && [ "$LAST_SEEN_2" != "$LAST_SEEN_1" ]; then
  ok "last_seen 在对话之后推进了（$LAST_SEEN_1 → $LAST_SEEN_2）"
else
  bad "last_seen 没推进（前后都是 '$LAST_SEEN_1'）：心跳可能停了"
fi

# ---------------------------------------------------------------------------
# 8 零入站端口
# ---------------------------------------------------------------------------
echo
echo "== 8 零入站端口 =="
# 只看**本脚本起的那些 afcli**（supervisor + worker）。同一台机器上别人的 afcli、
# 以及 afhub / opencode / docker 都不该算进这次验收的结论。
#
# **判据按「进程名 + pid」双锚定**，不单按 pid：`ss -tlnp` 列出的是整机所有监听，
# 只拿 pid 数字去 grep 会把**别的进程**的行也算进来——第一版就是这么红的：
# 命中的那一行是 `users:(("afhub",pid=830138,…))`，一个根本不是设备的进程。
# 名字也锚上，才是在问「afcli 这个进程有没有在听」。
NODE_PIDS=$(stray_pids | tr '\n' ' ')
if [ -z "$NODE_PIDS" ]; then
  bad "找不到设备进程，无法核对入站端口"
else
  ALT=$(echo "$NODE_PIDS" | tr ' ' '|' | sed 's/|$//')
  LISTENING=$(ss -tlnp 2>/dev/null \
    | grep -E "\(\"afcli[^\"]*\",pid=($ALT)," || true)
  if [ -z "$LISTENING" ]; then
    ok "设备进程（pid: $NODE_PIDS）没有开任何入站监听"
  else
    bad "设备进程开了入站监听：$LISTENING"
  fi
fi

# ---------------------------------------------------------------------------
# 收尾
# ---------------------------------------------------------------------------
echo
echo "== 汇总 =="
echo "  ✅ $pass 项通过 / ❌ $fail 项失败 / ⏭️  $skip 项跳过"
if [ "$skip" -gt 0 ]; then
  echo "  ⚠️  有 $skip 项**没跑**：它们不是通过，只是被跳过了。别把这份输出当成全绿。"
fi
if [ "$fail" -gt 0 ]; then
  echo "  现场在：$TMP"
  KEEP=1
  exit 1
fi
exit 0
