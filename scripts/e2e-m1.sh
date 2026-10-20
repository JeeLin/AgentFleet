#!/usr/bin/env bash
# M1 真设备端到端验收（里程碑《M1-web-console》子任务 14）。
#
# ## 它到底做了什么
#
# 起**真** afhub、起**真** afcli（`afcli run`，supervisor + worker 两个进程），
# 然后**按页面走的那条路**逐个标准断言：
#
#   页面 → Hub：`POST /api/*`（session cookie + `X-CSRF-Token`）
#   Hub → 页面：`GET /api/stream`（SSE，`channel.frame` / `device.status` / `session.state`）
#   Hub ↔ 设备：`/ws`（协议帧，由真 afcli 自己维持）
#
# 终端「从页面敲」用的就是 `lib/channel.ts` 的那三个调用
# （`POST /api/channels` → `POST /api/channels/{id}/send` → SSE `channel.frame`），
# **不是**在设备侧预置一条命令再断言「有输出」——预置命令能过，而 `/send`
# 到验收日可能一次都没被真跑过。
#
# ## 断言分级（与里程碑的一致）
#
# 自动断言：1、2、3、4、5、6、7、10 —— 失败即非零退出。
# 半自动（**只给观测值，不判通过**）：8、9 —— 开 3 个会话与拔网线的动作
# 在无人值守脚本里无法可靠重放，「内容不丢 / 不冻结」由人对照
# `docs/design-system.md` §7.3 的三态文案判断。脚本把它们伪装成 ✅ 就是撒谎。
#
# ## 用法
#
#   bash scripts/e2e-m1.sh                 # 全自动跑一遍，结束即清理
#   bash scripts/e2e-m1.sh --keep          # 保留日志现场（临时目录路径会打出来）
#   bash scripts/e2e-m1.sh --offline-drill # 追加标准 9 的状态机观测（约 1 分钟）
#   bash scripts/e2e-m1.sh --hold          # 跑完不清理，等你在浏览器里走查 8/9 后回车
#   bash scripts/e2e-m1.sh --port 19000    # 指定 Hub 端口
#
# 先决条件（真 afhub + 真 afcli + 浏览器；**不需要真树莓派**）见
# `scripts/e2e-m1/README.md`。**不改仓库里的 `data/`**：Hub 与设备的数据目录
# 都在本次运行的临时目录里（`AFHUB_DATA_DIR` / `AFCLI_DATA_DIR`）。
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

KEEP=0
OFFLINE_DRILL=0
HOLD=0
PORT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1 ;;
    --offline-drill) OFFLINE_DRILL=1 ;;
    --hold) HOLD=1 ;;
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
ok() { echo "  ✅ $1"; pass=$((pass + 1)); }
bad() { echo "  ❌ $1"; fail=$((fail + 1)); }
info() { echo "     $1"; }

# ---------------------------------------------------------------------------
# 现场清理：无论正常结束、断言失败还是 Ctrl-C，都不留 afhub / afcli 僵尸
# ---------------------------------------------------------------------------

# 本次运行的设备进程（supervisor + worker）。**按环境变量认，不按进程名认**：
# 同一台机器上别人也在跑 afcli 时，`pkill -f afcli` 会误伤别人的设备。
stray_pids() {
  [ -n "${TMP:-}" ] || return 0
  local p pid cmd envs
  for p in /proc/[0-9]*; do
    [ -r "$p/environ" ] || continue
    envs=$(tr '\0' '\n' <"$p/environ" 2>/dev/null) || continue
    case "$envs" in
      *"AFCLI_DATA_DIR=$TMP/node"*) ;;
      *) continue ;;
    esac
    cmd=$(tr '\0' ' ' <"$p/cmdline" 2>/dev/null) || continue
    case "$cmd" in
      *"afcli"*) pid=${p#/proc/}; echo "$pid" ;;
    esac
  done
}

cleanup() {
  local rc=$?
  trap - EXIT INT TERM
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
for tool in curl jq ss sqlite3 awk sed; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
  echo "  ❌ 缺工具：$missing（标准 10 要 sqlite3，标准 7 要 ss）" >&2
  exit 2
fi
echo "  ✅ 工具齐备（curl / jq / ss / sqlite3）"

AFHUB_BIN=${AFHUB_BIN:-$ROOT/target/debug/afhub}
AFCLI_BIN=${AFCLI_BIN:-$ROOT/target/debug/afcli}
if [ ! -x "$AFHUB_BIN" ] || [ ! -x "$AFCLI_BIN" ]; then
  echo "  ⏳ 二进制不存在，先构建（CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}）"
  CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2} cargo build --bin afhub --bin afcli || {
    echo "  ❌ 构建失败：验收无法开始" >&2
    exit 2
  }
fi
if [ ! -x "$AFHUB_BIN" ] || [ ! -x "$AFCLI_BIN" ]; then
  echo "  ❌ 找不到 $AFHUB_BIN / $AFCLI_BIN（用 AFHUB_BIN / AFCLI_BIN 指定）" >&2
  exit 2
fi
echo "  ✅ 验收对象：$AFHUB_BIN / $AFCLI_BIN"
info "afcli：$("$AFCLI_BIN" --version 2>&1 | head -1)"
info "构建时间：$(date -r "$AFHUB_BIN" '+%F %T')（hub）/ $(date -r "$AFCLI_BIN" '+%F %T')（afcli）"
info "本机：$(uname -sm)，内核 $(uname -r)"

# ---------------------------------------------------------------------------
# 现场：临时目录 + 工作区夹具 + 端口
# ---------------------------------------------------------------------------
TMP_BASE=${TMPDIR:-/tmp}
mkdir -p "$TMP_BASE" 2>/dev/null || TMP_BASE=/tmp
TMP=$(mktemp -d "$TMP_BASE/agentfleet-e2e-m1.XXXXXX") || {
  echo "  ❌ 无法创建临时目录（TMPDIR=$TMP_BASE）" >&2
  exit 2
}
mkdir -p "$TMP/hub" "$TMP/node" "$TMP/ws/src" "$TMP/outside"

WS=$TMP/ws
printf 'hello from the e2e workspace\n' >"$WS/a.txt"
printf 'inner file\n' >"$WS/src/inner.txt"
printf 'readme body\n' >"$WS/README.md"
# 三种越界构造里的两种要真东西在界外：`..` 逃逸的落点、符号链接的目标
printf 'outside the workspace\n' >"$TMP/outside/secret.txt"
ln -s "$TMP/outside/secret.txt" "$WS/link-outside.txt"

HUB_LOG=$TMP/hub.log
SSE_LOG=$TMP/sse.log
COOKIES=$TMP/cookies.txt
BODY=$TMP/last-body.json
NODE_LOG_DIR=$TMP/node/agentfleet

if [ -z "$PORT" ]; then
  for _ in $(seq 1 20); do
    PORT=$(((RANDOM % 2000) + 18000))
    ss -ltn 2>/dev/null | grep -q ":$PORT " || break
  done
fi
BASE="http://127.0.0.1:$PORT"
echo "  现场：$TMP（Hub 端口 $PORT，工作区 $WS）"

# ---------------------------------------------------------------------------
# 终端初值从被测代码读，夹具不重写一遍：自己写死再断言「页面给的尺寸到了 PTY」
# 是自我实现。读不到就退出，不退回写死的值——那正是要防的事。
# ---------------------------------------------------------------------------
PAGE_PARAMS_SRC="$ROOT/packages/web/src/pages/SessionPage.vue"
page_term_cols() {
  grep -o 'cols: [0-9]\+' "$PAGE_PARAMS_SRC" | head -1 | grep -o '[0-9]\+'
}
page_term_rows() {
  grep -o 'rows: [0-9]\+' "$PAGE_PARAMS_SRC" | head -1 | grep -o '[0-9]\+'
}
PAGE_COLS=$(page_term_cols)
PAGE_ROWS=$(page_term_rows)
if [ -z "$PAGE_COLS" ] || [ -z "$PAGE_ROWS" ]; then
  echo "  ❌ 读不到页面的终端初值（$PAGE_PARAMS_SRC 的 cols:/rows:）" >&2
  exit 2
fi
echo "  页面终端初值（从 SessionPage.vue 读出）：${PAGE_COLS}x${PAGE_ROWS}"

# ---------------------------------------------------------------------------
# 起 Hub（真 afhub 进程）
# ---------------------------------------------------------------------------
AFHUB_ADDR="127.0.0.1:$PORT" AFHUB_DATA_DIR="$TMP/hub" RUST_LOG=${RUST_LOG:-info} \
  "$AFHUB_BIN" >"$HUB_LOG" 2>&1 &
HUB_PID=$!
hub_up=0
for _ in $(seq 1 200); do
  if curl -sf "$BASE/healthz" >/dev/null 2>&1; then
    hub_up=1
    break
  fi
  kill -0 "$HUB_PID" 2>/dev/null || break
  sleep 0.1
done
if [ "$hub_up" -ne 1 ]; then
  echo "  ❌ afhub 没起来（端口 $PORT）" >&2
  sed 's/\x1b\[[0-9;]*m//g' "$HUB_LOG" | tail -20 | sed 's/^/       /' >&2
  exit 2
fi

# 首启：设密码 → 登录拿 cookie（`af_session` + `af_csrf`）
SETUP_TOKEN=$(tr -d '\n' <"$TMP/hub/setup-token.txt")
curl -sS -o "$BODY" -X POST "$BASE/setup" -H "x-setup-token: $SETUP_TOKEN" \
  -H 'content-type: application/json' -d '{"password":"correct horse battery"}' >/dev/null
curl -sS -o "$BODY" -c "$COOKIES" -X POST "$BASE/login" -H 'content-type: application/json' \
  -d '{"password":"correct horse battery"}' >/dev/null
CSRF=$(awk '$6 == "af_csrf" {print $7}' "$COOKIES")
if [ -z "$CSRF" ]; then
  echo "  ❌ 登录没拿到 af_csrf（Hub 日志见 $HUB_LOG）" >&2
  sed 's/\x1b\[[0-9;]*m//g' "$HUB_LOG" | tail -20 | sed 's/^/       /' >&2
  exit 2
fi
echo "  ✅ afhub 就绪（${BASE}），已登录（CSRF 已拿到）"

# ---------------------------------------------------------------------------
# 页面侧数据面：SSE 订阅（浏览器 EventSource 的那条）
# ---------------------------------------------------------------------------
: >"$SSE_LOG"
curl -N -s -b "$COOKIES" "$BASE/api/stream" >>"$SSE_LOG" 2>&1 &
SSE_PID=$!

# ---------------------------------------------------------------------------
# 起设备（真 afcli，一条命令）
# ---------------------------------------------------------------------------
NODE_CMD="AFCLI_DATA_DIR=$TMP/node AFCLI_HUB_URL=ws://127.0.0.1:$PORT/ws $AFCLI_BIN run"
AFCLI_DATA_DIR="$TMP/node" AFCLI_HUB_URL="ws://127.0.0.1:$PORT/ws" \
  setsid "$AFCLI_BIN" run >>"$TMP/node.stdout" 2>&1 &
NODE_PID=$!

NODE_ID=""
DEVICE_STATUS=""
for _ in $(seq 1 200); do
  DEVICES=$(curl -s -b "$COOKIES" "$BASE/api/devices")
  NODE_ID=$(printf '%s' "$DEVICES" | jq -r '.[0].id // empty' 2>/dev/null)
  DEVICE_STATUS=$(printf '%s' "$DEVICES" | jq -r '.[0].status // empty' 2>/dev/null)
  [ -n "$NODE_ID" ] && [ "$DEVICE_STATUS" = "online" ] && break
  sleep 0.2
done

# ---------------------------------------------------------------------------
# 断言用的小工具（全部只读现场文件，不碰被测进程）
# ---------------------------------------------------------------------------

# `METHOD PATH [BODY]` → HTTP_CODE / BODY 两个全局量
#
# **不打印 stdout**：`open_channel()` 靠「本函数只写全局量、stdout 干净」
# 来输出 channel_id（它自己 `printf | jq`）。在这里加一句 `printf "$HTTP_CODE"`
# 会把状态码送进 stdout，`$(open_channel ...)` 捕获成 `201<channel_id>`——
# 于是 URL 里多一个 201 前缀，每个 `/send` 与 `/resize` 都变成 404。
#
# 要读返回码用 `api_code`（下面那个），它把状态码打到 stdout。
api() {
  local method=$1 path=$2 data=${3-}
  if [ -n "$data" ]; then
    HTTP_CODE=$(curl -sS -o "$BODY" -w '%{http_code}' -X "$method" -b "$COOKIES" \
      -H "x-csrf-token: $CSRF" -H 'content-type: application/json' "$BASE$path" --data-binary "$data")
  else
    HTTP_CODE=$(curl -sS -o "$BODY" -w '%{http_code}' -X "$method" -b "$COOKIES" \
      -H "x-csrf-token: $CSRF" "$BASE$path")
  fi
  LAST_BODY=$(cat "$BODY" 2>/dev/null)
  return 0
}

# 同 `api()`，但**把状态码打到 stdout**：给 `rcode=$(api_code POST ...)` 用。
#
# 为什么要有两个：命令替换拿到的是子 shell 的 stdout，而 `LAST_BODY` /
# `HTTP_CODE` 的赋值发生在同一个子 shell 里——父 shell 的 `LAST_BODY`
# **不会**被更新，它留着上一次的值。所以「打印响应体」那条失败分支
# 看到的永远是陈旧内容，别拿它当本次请求的响应体。
api_code() {
  api "$@"
  printf '%s' "$HTTP_CODE"
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

# 某条通道收到的原始字节（PTY 输出 / 文本帧），拼成一段文本
bytes_of() {
  frames_of "$1" | jq -r 'select(.bytes != null) | .bytes' 2>/dev/null
}

# `POST /api/channels`（与 SessionPage.vue 的 ensureChannel 逐字一致）→ 打印 channel_id
open_channel() {
  api POST /api/channels "{\"session_id\":\"$SID\",\"connector\":\"$1\",\"params\":$2}"
  printf '%s' "$LAST_BODY" | jq -r '.channel_id // empty' 2>/dev/null
}

# `/send`：裸字节上行（页面键盘输入 / files 取数都走这条）
send_bytes() {
  curl -sS -o "$BODY" -w '%{http_code}' -b "$COOKIES" -H "x-csrf-token: $CSRF" \
    -X POST "$BASE/api/channels/$1/send" --data-binary "$2"
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

# ---- 断言判据（wait_until 的条件函数）------------------------------------
# 都用「当轮设成全局」的通道号 / 标记：条件函数要传给 wait_until，参数只能走全局。
terminal_has_frames() { [ "$(frames_of "$TCH" | wc -l)" -ge 1 ]; }
stty_size_is() { bytes_of "$1" | grep -q "$2"; }
# `stty size` 输出的是 `rows cols`，与 open 参数的 cols/rows 顺序相反。
stty_size_is_page_default() { stty_size_is "$TCH" "$PAGE_ROWS $PAGE_COLS"; }
# 132x40 是这一步自己的输入，写死是对的：它验的是 Hub 把 resize 转成 TIOCSWINSZ。
stty_size_is_40_132() { stty_size_is "$TCH" '40 132'; }
# ≥2 次才说明「命令真跑了」：一次是 PTY 把敲进去的字符回显，第二次才是 echo 的输出。
# 只断言「有输出」的话，设备侧预置一条命令就能过——那正是本标准的断点所在。
marker_typed() { [ "$(bytes_of "$TCH" | grep -o "$MARK" | wc -l)" -ge 2 ]; }
files_listed() { frames_of "$FCH" | jq -r 'select(.op=="list") | .entries[]?.name' 2>/dev/null | grep -qx 'a.txt'; }
file_read() { frames_of "$FCH" | jq -r 'select(.op=="read") | .text // empty' 2>/dev/null | grep -q 'hello from the e2e workspace'; }
exec_result() { [ "$(frames_of "$ECH" | jq -r 'select(.exit_code != null) | .exit_code' 2>/dev/null | grep -c .)" -ge 1 ]; }

# 页面上看得见的「拒绝」信号：通道帧里带 error/code/reason，或一条 channel.error 事件。
# 页面看不到的错误 = 用户看不到的错误（判定标准 5 的「错误可读」时按这个算）。
rejections_visible() {
  {
    frames_of "$1" | jq -r 'select((.error != null) or (.code != null) or (.reason != null)) | tostring' 2>/dev/null
    sse_lines channel.error | cut -f2- | jq -r --arg c "$1" 'select(.channel_id == $c) | tostring' 2>/dev/null
  } | grep -c . || true
}
REJ_BEFORE=0
rejection_new() { [ "$(rejections_visible "$FCH")" -gt "$REJ_BEFORE" ]; }

# 断言失败时把现场摊开：日志、事件、通道帧数。诊断信息不是装饰——
# 一条「断言失败」而没有现场，等于让人重新手工跑一遍。
dump() {
  local what=$1
  echo "     —— 现场（$what）——"
  echo "     Hub 日志里与通道/错误有关的行（末 8 行）："
  sed 's/\x1b\[[0-9;]*m//g' "$HUB_LOG" | grep -E 'channel|WARN|ERROR' | tail -8 | sed 's/^/       /'
  if [ -f "$NODE_LOG_DIR/logs/afcli.log" ]; then
    echo "     设备日志（末 5 行）："
    tail -5 "$NODE_LOG_DIR/logs/afcli.log" | sed 's/^/       /'
  fi
  echo "     SSE：$(sse_lines channel.frame | wc -l) 条 channel.frame、$(sse_lines channel.ready | wc -l) 条 channel.ready、$(sse_lines channel.closed | wc -l) 条 channel.closed"
}

# ---------------------------------------------------------------------------
echo
echo "== 标准 1 afcli run 一条命令接入，设备出现在 /api/devices =="
echo "  接入命令：$NODE_CMD"
if [ -n "$NODE_ID" ] && [ "$DEVICE_STATUS" = "online" ]; then
  ok "设备已在 /api/devices：id=$NODE_ID status=$DEVICE_STATUS"
  info "$(printf '%s' "$DEVICES" | jq -c '.[0] | {id, name, platform, agent_version, status, key_fingerprint}')"
  supervisor_cmd=$(tr '\0' ' ' <"/proc/$NODE_PID/cmdline" 2>/dev/null)
  worker_pid=$(pgrep -P "$NODE_PID" | head -1)
  WORKER_PID=$worker_pid
  if [ -n "$worker_pid" ]; then
    ok "supervisor/worker 两层都在（约束 12）：supervisor=$NODE_PID「$supervisor_cmd」worker=$worker_pid「$(tr '\0' ' ' <"/proc/$worker_pid/cmdline" 2>/dev/null)」"
  else
    bad "supervisor 起来了但没看到 worker 子进程（约束 12 要求两层）"
    dump "worker 缺失"
  fi
  if printf '%s' "$DEVICES" | jq -e '.[0].key_fingerprint | type == "string"' >/dev/null; then
    fp=$(printf '%s' "$DEVICES" | jq -r '.[0].key_fingerprint')
    case "$fp" in
      *'***'*) ok "API 只给指纹（约束 13）：$fp" ;;
      *) bad "指纹形状不对（期望前 3 + *** + 后 3）：$fp" ;;
    esac
    if printf '%s' "$DEVICES" | jq -e '.[0] | has("public_key") or has("private_key")' >/dev/null; then
      bad "设备响应里出现了密钥字段（约束 6/13）"
    else
      ok "响应里没有密钥材料（约束 13）"
    fi
  fi
else
  bad "设备没在 30s 内变成 online（status=${DEVICE_STATUS:-<无>}）"
  dump "设备未上线"
fi

# ---------------------------------------------------------------------------
echo
echo "== 标准 2 建会话，会话出现在 /api/sessions =="
SID=""
api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS\"}"
if [ "$HTTP_CODE" = "201" ]; then
  SID=$(printf '%s' "$LAST_BODY" | jq -r '.id')
  ok "POST /api/sessions → 201，session_id=$SID"
  if [ "$(printf '%s' "$LAST_BODY" | jq -r '.workspace.path')" = "$WS" ] &&
    [ "$(printf '%s' "$LAST_BODY" | jq -r '.workspace.node_id')" = "$NODE_ID" ] &&
    [ "$(printf '%s' "$LAST_BODY" | jq -r '.state')" = "running" ]; then
    ok "会话绑定了 (node_id, path) 且状态 running：$(printf '%s' "$LAST_BODY" | jq -c '{id,state,workspace}')"
  else
    bad "会话的工作区/状态与请求不符：$LAST_BODY"
  fi
  api GET /api/sessions
  if printf '%s' "$LAST_BODY" | jq -e --arg s "$SID" 'map(.id) | index($s) != null' >/dev/null; then
    ok "GET /api/sessions 里能看到它（共 $(printf '%s' "$LAST_BODY" | jq 'length') 个会话）"
  else
    bad "GET /api/sessions 里没有 $SID：$LAST_BODY"
  fi
else
  bad "POST /api/sessions → $HTTP_CODE：$LAST_BODY"
  dump "建会话失败"
fi

if [ -z "$SID" ]; then
  echo
  echo "  ❌ 没有会话，后面的标准都无法建立通道 —— 停在这里。"
else
  # -------------------------------------------------------------------------
  echo
  echo "== 标准 3 终端面板：从页面敲 echo 有输出，stty size 验证 resize =="
  TCH=$(open_channel terminal "{\"cwd\":\"$WS\",\"cols\":$PAGE_COLS,\"rows\":$PAGE_ROWS}")
  if [ -z "$TCH" ]; then
    bad "POST /api/channels(terminal) 没拿到 channel_id：$HTTP_CODE $LAST_BODY"
    dump "终端通道建立失败"
  else
    ok "终端通道已建立（channel_id=$TCH，cwd=工作区、${PAGE_COLS}x${PAGE_ROWS}）"
    # 页面在 Hub 一发 `channel.ready` 就显示「已连接」，而设备此刻可能还没 open
    # （那一瞬间的 /send 会被设备当成未知通道静默丢弃）。等人能看到的第一个
    # 证据——提示符帧——再开始敲，与人手的速度一致。
    if wait_until 20 terminal_has_frames; then
      info "设备已开通道（首帧到达，$(frames_of "$TCH" | wc -l) 帧）"
    else
      info "20s 内没等到任何首帧，仍然继续（后面的断言会给出更具体的失败）"
    fi
    MARK="AF_E2E_${RANDOM}${RANDOM}"
    # 末尾那个换行必须是真字节：`$(printf '\n')` 会被命令替换吃掉，发过去就是
    # 一句没有回车的台词——PTY 回显了它，但 shell 永远等不到 Enter。
    send_bytes "$TCH" 'stty size'$'\n' >/dev/null
    if wait_until 15 stty_size_is_page_default; then
      ok "页面给的 ${PAGE_COLS}x${PAGE_ROWS} 到了 PTY：\`stty size\` → $PAGE_ROWS $PAGE_COLS"
    else
      bad "没等到 \`stty size\` 的 $PAGE_ROWS $PAGE_COLS（页面给的尺寸没进 PTY？）"
      dump "终端首帧"
    fi
    rcode=$(api_code POST "/api/channels/$TCH/resize" '{"cols":132,"rows":40}')
    if [ "$rcode" = "204" ]; then
      ok "POST /api/channels/$TCH/resize → 204"
    else
      # 不打 $LAST_BODY：它在命令替换的子 shell 里赋值，父 shell 这份是
      # 上一次的值（多半是 GET /api/sessions 的数组），印出来只会误导。
      bad "resize 请求失败：HTTP $rcode（响应体见 $BODY）"
    fi
    # 末尾那个换行必须是真字节：`$(printf '\n')` 会被命令替换吃掉，发过去就是
    # 一句没有回车的台词——PTY 回显了它，但 shell 永远等不到 Enter。
    send_bytes "$TCH" 'stty size'$'\n' >/dev/null
    if wait_until 15 stty_size_is_40_132; then
      ok "resize 生效：POST /resize 132x40 后 \`stty size\` → 40 132"
    else
      bad "resize 后 \`stty size\` 没变成 40 132"
      dump "resize"
    fi
    code=$(send_bytes "$TCH" "echo $MARK"$'\n')
    info "POST /send（= 页面上敲下这一行）→ $code：echo $MARK"
    if wait_until 20 marker_typed; then
      ok "从页面敲的 echo 有输出：$MARK 出现 $(bytes_of "$TCH" | grep -o "$MARK" | wc -l) 次（≥2 = 回显 + 命令真的跑了）"
      info "输出片段：$(bytes_of "$TCH" | tr -d '\000' | tr '\r\n' ' ' | grep -o ".\{0,40\}$MARK.\{0,20\}" | tail -1)"
    else
      bad "从页面敲的 echo 没等到输出（$MARK 出现 $(bytes_of "$TCH" | grep -o "$MARK" | wc -l) 次）"
      dump "终端 /send"
    fi
  fi
fi

# ---------------------------------------------------------------------------
if [ -n "$SID" ]; then
  echo
  echo "== 标准 4 文件面板：浏览工作区目录树、读文件 =="
  info "通道参数与 SessionPage.vue 的 ensureChannel 逐字一致：files → params={}（工作区根由 Hub 按会话补）"
  FCH=$(open_channel files '{}')
  if [ -z "$FCH" ]; then
    bad "POST /api/channels(files) 没拿到 channel_id：$HTTP_CODE $LAST_BODY"
    dump "文件通道建立失败"
  else
    ok "文件通道已建立（channel_id=$FCH）"
    code=$(send_bytes "$FCH" '{"op":"list","path":"."}')
    if wait_until 10 files_listed; then
      ok "列目录成功：$(frames_of "$FCH" | jq -r 'select(.op=="list") | .entries | map(.name) | join(",")' | head -1)"
    else
      bad "列目录没有回帧（/send → $code；该通道共 $(frames_of "$FCH" | wc -l) 条 channel.frame）——页面会一直是一棵空树"
      dump "files list"
    fi
    code=$(send_bytes "$FCH" '{"op":"read","path":"a.txt"}')
    if wait_until 10 file_read; then
      ok "读文件成功：a.txt → $(frames_of "$FCH" | jq -r 'select(.op=="read" and .path=="a.txt") | .text' | head -1)"
    else
      bad "读文件没有回帧（/send → $code；该通道共 $(frames_of "$FCH" | wc -l) 条 channel.frame）"
      dump "files read"
    fi
  fi

  # -------------------------------------------------------------------------
  echo
  echo "== 标准 5 越界路径被拒且错误可读（.. 逃逸 / 绝对路径 / 符号链接逃逸）=="
  if [ -z "${FCH:-}" ]; then
    bad "没有文件通道，三种构造都发不出去"
  else
    rej_fail=0
    for probe in "../outside/secret.txt" "/etc/hostname" "link-outside.txt"; do
      REJ_BEFORE=$(rejections_visible "$FCH")
      code=$(send_bytes "$FCH" "$(jq -cn --arg p "$probe" '{op:"read",path:$p}')")
      if wait_until 8 rejection_new; then
        ok "拒绝「$probe」且页面能看到：$(frames_of "$FCH" | jq -r 'select(.reason != null) | .reason' | tail -1)"
      else
        bad "拒绝「$probe」在页面上看不见（/send → $code、错误信号 $REJ_BEFORE → $(rejections_visible "$FCH")）"
        rej_fail=$((rej_fail + 1))
      fi
    done
    if [ "$rej_fail" -gt 0 ]; then
      info "取数与错误全走 POST /api/channels/{id}/send，而该端点把请求体包成 {\"bytes\":…}（terminal 载荷）；"
      info "files connector 只认顶层 {\"op\":…}，所以在设备的边界检查之前就已经走岔了。详见 README「已知缺口」。"
    fi
  fi

  # -------------------------------------------------------------------------
  echo
  echo "== 标准 6 exec 面板：一次性命令，退出码与输出正确 =="
  info "通道参数与 SessionPage.vue 的 runCommand 逐字一致：{command,args,cwd:\"\"}（工作区根由 Hub 按会话补）"
  ECH=$(open_channel exec '{"command":"sh","args":["-c","echo OUT; echo ERR >&2; exit 7"],"cwd":""}')
  if [ -z "$ECH" ]; then
    bad "POST /api/channels(exec) 没拿到 channel_id：$HTTP_CODE $LAST_BODY"
    dump "exec 通道建立失败"
  elif wait_until 10 exec_result; then
    got=$(frames_of "$ECH" | jq -c 'select(.exit_code != null)' | head -1)
    ok "exec 结果：$got"
    if [ "$(frames_of "$ECH" | jq -r 'select(.exit_code != null) | .exit_code' | head -1)" = "7" ] &&
      [ "$(frames_of "$ECH" | jq -r 'select(.exit_code != null) | .stdout_b64' | head -1 | base64 -d 2>/dev/null)" = "OUT" ] &&
      [ "$(frames_of "$ECH" | jq -r 'select(.exit_code != null) | .stderr_b64' | head -1 | base64 -d 2>/dev/null)" = "ERR" ]; then
      ok "退出码 7、stdout=OUT、stderr=ERR 全对"
    else
      bad "结果字段不对：$got"
    fi
  else
    bad "exec 通道没有回帧（该通道共 $(frames_of "$ECH" | wc -l) 条 channel.frame）——页面上「跑一条命令」不会有任何反应"
    dump "exec"
  fi
else
  echo
  echo "== 标准 4 / 5 / 6 =="
  bad "没有会话，无法建通道（见标准 2）"
fi

# ---------------------------------------------------------------------------
echo
echo "== 标准 7 零入站端口：设备侧不开放任何监听 =="
node_pids="$(pgrep -P "$NODE_PID" 2>/dev/null | tr '\n' ' ')"
pid_alt=$NODE_PID
for p in $node_pids; do pid_alt="$pid_alt|$p"; done
if [ -z "$node_pids" ]; then
  info "worker 子进程不在（supervisor 可能正在重启它），只核对 supervisor"
fi
listeners=""
for pid in $NODE_PID $node_pids; do
  hit=$(ss -tlnp 2>/dev/null | grep -F "pid=$pid," || true)
  [ -n "$hit" ] && listeners="$listeners$hit
"
done
if [ -z "$listeners" ]; then
  ok "afcli（supervisor=$NODE_PID worker=${node_pids:-—}）没有任何 LISTEN 套接字"
else
  bad "设备侧出现了入站监听（约束 1）："
  printf '%s' "$listeners" | sed 's/^/       /'
fi
outbound=$(ss -tnp 2>/dev/null | grep -E "pid=($pid_alt)," | grep -v LISTEN || true)
if [ -n "$outbound" ]; then
  ok "设备只有主动外联（到 Hub 的 $PORT 端口）："
  printf '%s\n' "$outbound" | sed 's/^/       /'
else
  bad "没看到设备的出站连接（WS 断了？）"
fi
hub_listen=$(ss -tlnp 2>/dev/null | grep -F ":$PORT " || true)
case "$hub_listen" in
  *127.0.0.1:$PORT*) ok "Hub 自身只监听回环：$(printf '%s' "$hub_listen" | tr -s ' ')" ;;
  *) bad "Hub 的监听不是回环：$hub_listen" ;;
esac

# ---------------------------------------------------------------------------
echo
echo "== 标准 10 心跳能力表与 node 实际实现的 connector 完全一致 =="
REPORTED=$(sqlite3 "$TMP/hub/agentfleet.db" "select capabilities from device where id='$NODE_ID';" 2>/dev/null)
if [ -z "$REPORTED" ]; then
  bad "读不到 device.capabilities（sqlite3 $TMP/hub/agentfleet.db）"
  dump "capabilities"
else
  info "上报值（device.capabilities）：$REPORTED"
  got=$(printf '%s' "$REPORTED" | jq -r '.connectors | sort | join(",")')
  want="exec,files,terminal"
  if [ "$got" = "$want" ]; then
    ok "上报的 connectors == 注册表：$want"
  else
    bad "上报的 connectors（$got）与注册表（$want）不一致 —— 上报了没实现会让路由把任务派给做不到的设备"
  fi
  if [ "$(printf '%s' "$REPORTED" | jq -r '.agents | length')" = "0" ]; then
    ok "agents 为空（M1 无 agent，ACP 归 M3）"
  else
    bad "M1 不该上报 agent：$(printf '%s' "$REPORTED" | jq -c '.agents')"
  fi
  # 能力硬过滤的两个方向都用真通道验一次
  for c in terminal files exec; do
    api POST /api/channels "{\"session_id\":\"$SID\",\"connector\":\"$c\",\"params\":{}}"
    if [ "$HTTP_CODE" = "201" ]; then
      ok "上报的 connector 「$c」能被下发（Hub 没把它当未上报）"
      api DELETE "/api/channels/$(printf '%s' "$LAST_BODY" | jq -r '.channel_id')"
    else
      bad "上报的 connector 「$c」被 Hub 拒了：$HTTP_CODE $LAST_BODY"
    fi
  done
  api POST /api/channels "{\"session_id\":\"$SID\",\"connector\":\"git\",\"params\":{}}"
  if [ "$HTTP_CODE" = "409" ] && [ "$(printf '%s' "$LAST_BODY" | jq -r '.error')" = "connector_not_reported" ]; then
    ok "没上报的 connector 「git」在 Hub 就被拒（409 connector_not_reported，不下发注定被拒的 open_channel）"
  else
    bad "未上报的 connector 没被拒：$HTTP_CODE $LAST_BODY"
  fi
fi

# ---------------------------------------------------------------------------
# 8 / 9：半自动。**给观测值，不判通过**（里程碑明写不把人工步骤伪装成自动断言）
# ---------------------------------------------------------------------------
echo
echo "== 标准 8 多会话 tab（半自动：下面是观测值，不是断言）=="
ready_before=$(sse_lines channel.ready | wc -l)
closed_before=$(sse_lines channel.closed | wc -l)
OBS_SIDS="$SID"
for i in 2 3; do
  api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS\"}"
  s=$(printf '%s' "$LAST_BODY" | jq -r '.id // empty')
  [ -n "$s" ] || continue
  OBS_SIDS="$OBS_SIDS $s"
  old=$SID
  SID=$s
  ch=$(open_channel terminal "{\"cwd\":\"$WS\",\"cols\":$PAGE_COLS,\"rows\":$PAGE_ROWS}")
  SID=$old
  [ -n "$ch" ] || continue
  m="AF_TAB${i}_${RANDOM}"
  send_bytes "$ch" "echo $m"$'\n' >/dev/null
  sleep 1.5
  info "会话 $s：通道 $ch，敲入 $m → 该通道 $(frames_of "$ch" | wc -l) 条帧，$m 出现 $(bytes_of "$ch" | grep -o "$m" | wc -l) 次"
done
info "本轮的通道账（只看这一段）：channel.ready +$(( $(sse_lines channel.ready | wc -l) - ready_before )) 条、channel.closed +$(( $(sse_lines channel.closed | wc -l) - closed_before )) 条"
info "会话数：$(printf '%s\n' $OBS_SIDS | grep -c .)"
info "人工判断（本脚本不判）：在浏览器里开 3 个 tab、各开不同面板、来回切换——通道数不减、"
info "                   切回来时终端/文件内容还在（对照 docs/design-system.md §7.3）。"
info "                   脚本能给的只有上面这几个计数，它看不到你的屏幕上有没有内容。"

echo
echo "== 标准 9 拔网线 → 离线 → 恢复（半自动：观测值 + 步骤，不是断言）=="
if [ "$OFFLINE_DRILL" -eq 1 ]; then
  drill_pid=$(pgrep -P "$NODE_PID" | head -1)
  if [ -z "$drill_pid" ]; then
    info "worker 不在，跳过演练"
  else
    info "演练：SIGSTOP 设备 worker（pid $drill_pid）= 设备不再回应，每 2s 读一次 /api/devices.status"
    kill -STOP "$drill_pid"
    t0=$SECONDS
    while [ $((SECONDS - t0)) -lt 60 ]; do
      st=$(curl -s -b "$COOKIES" "$BASE/api/devices" | jq -r '.[0].status')
      info "  t+$((SECONDS - t0))s status=$st"
      [ "$st" = "online" ] || break
      sleep 2
    done
    info "SIGCONT → 继续观察回到 online（最多 60s）"
    kill -CONT "$drill_pid"
    t1=$SECONDS
    while [ $((SECONDS - t1)) -lt 60 ]; do
      st=$(curl -s -b "$COOKIES" "$BASE/api/devices" | jq -r '.[0].status')
      info "  t+$((SECONDS - t1))s status=$st"
      [ "$st" = "online" ] && break
      sleep 2
    done
  fi
  info "本轮 device.status 事件：$(sse_lines device.status | cut -f2- | jq -r '.status' | tr '\n' ' ')"
else
  info "（用 --offline-drill 可让脚本代跑「设备停止回应」并打印状态机观测值）"
fi
info "人工判断（本脚本不判）：终端在这一段里显示「重连中 · 不冻结」——既不是空白也不是冻结；"
info "                   恢复后自动重连、不用刷新页面（对照 docs/design-system.md §7.3 的三态文案）。"

# ---------------------------------------------------------------------------
if [ "$HOLD" -eq 1 ]; then
  echo
  echo "== 现场保持运行（--hold）=="
  info "浏览器打开：$BASE    密码：correct horse battery"
  info "Hub 数据目录：$TMP/hub；设备数据目录：$TMP/node"
  info "在上面把标准 8（3 个 tab 来回切）与标准 9（拔网线）走一遍，然后按回车清理；"
  info "Ctrl-C 也可以——trap 一样会收进程。"
  read -r _ || true
  KEEP=1
fi

echo
echo "== 汇总 =="
echo "  自动断言：✅ $pass 项通过，❌ $fail 项失败"
echo "  半自动：标准 8（多会话 tab）、标准 9（拔网线）—— 上面只给了观测值，判断权在人"
if [ "$KEEP" -eq 1 ]; then echo "  现场：$TMP"; fi
if [ "$fail" -gt 0 ]; then
  echo "  ⚠️  验收未通过：$fail 项断言失败（诊断见上面各处的「现场」）"
  exit 1
fi
echo "  验收通过（端到端可自动判定的部分全绿；8/9 仍需人工浏览器走查）"
