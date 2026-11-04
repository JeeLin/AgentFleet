#!/usr/bin/env bash
# v0.5.0 真设备端到端验收（里程碑《v0.5.0-opencode-layout》子任务 6）。
#
# ## 它验的是什么，以及为什么要走到真设备才算验过
#
# v0.5.0 是**纯前端**的布局重排（右侧一格、＋ 换工具、左右一条分栏、窄屏抽屉），
# 所以本脚本**不验像素**——那是 `SessionPage.test.ts` 的活，而且 DOM 测试比
# 截图稳定得多。本脚本验的是**布局重排里那条有真实代价的语义**：
#
#   「换工具 = 替换 + 释放被换掉那个的通道」
#
# 这条语义的唯一证据在**设备侧**：`close_channel` 到了设备之后，
# `crates/node-connector/src/connector/terminal.rs:305` 的 `pty.close()` 会**真的杀掉
# 用户的 shell**。DOM 测试只能证明页面发了 `DELETE /api/channels/{id}`
# （`deletes()` 那条 mock 断言），证明不了「设备上那条 PTY 真的没了」。
# 所以这里起真 afhub、真 afcli、真 PTY，按页面走的那条路逐条断言：
#
#   页面 → Hub：`POST /api/channels`（开通道）
#               `DELETE /api/channels/{id}`（换工具时释放被换掉的那条）
#   Hub → 设备：`/ws` 上的 `open_channel` / `close_channel`（由真 afcli 自己维持）
#   设备 → PTY：`node-core` 的 terminal connector 亲自 spawn shell
#
# ## 断言与里程碑 §7.1.3 的对应
#
# §7.1.3 明写「换工具会释放旧通道」是**已知降级、不缓解**，并要求「e2e 断言
# 换工具后旧工具真的没了」。本脚本的断言 4/5/7 就是那三条：
#
#   4  换工具 → 旧通道 204，且旧通道**再也发不进去**（服务端已不认它）
#   5  「＋」选回自己 → **一个通道调用都不发**（点当前那个零副作用）
#   6  每次替换都释放，且**只**释放被换掉那一个
#   7  收起右侧一格 → 名下通道全部释放（退出即释放）
#
# ## 断言分级
#
# 全部自动断言，失败即非零退出。没有「跳过」档：先决条件（一个能 spawn shell 的
# 环境）本机必然满足，缺了就是环境坏了（退出码 2），不该画成 ⏭️。
#
# ## 用法
#
#   bash scripts/e2e-v0.5.0.sh              # 全自动跑一遍，结束即清理
#   bash scripts/e2e-v0.5.0.sh --keep       # 保留日志现场（临时目录路径会打出来）
#   bash scripts/e2e-v0.5.0.sh --port 19000 # 指定 Hub 端口
#
# 先决条件与两个已知坑见 `scripts/e2e-v0.5.0/README.md`。**不改仓库里的 `data/`**：
# Hub 与设备的数据目录、以及那个临时工作区，都在本次运行的临时目录里。
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

KEEP=0
PORT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1 ;;
    --port)
      PORT=${2:-}
      shift
      ;;
    -h | --help)
      sed -n '2,50p' "$0"
      exit 0
      ;;
    *)
      echo "未知参数：$1" >&2
      exit 2
      ;;
  esac
  shift
done

pass=0
fail=0
ok() {
  pass=$((pass + 1))
  echo "  ✅ $*"
}
bad() {
  fail=$((fail + 1))
  echo "  ❌ $*" >&2
}
info() { echo "     $*"; }

# ---------------------------------------------------------------------------
# 清理：先杀进程再删目录；`--keep` 时留下现场。
# ---------------------------------------------------------------------------
cleanup() {
  local pid
  for pid in "${SSE_PID:-}" "${NODE_PID:-}" "${HUB_PID:-}"; do
    [ -n "$pid" ] || continue
    # 设备是 `setsid` 起的（supervisor + worker 一进程组），要杀整组，
    # 只杀 $! 会留下 worker 继续占着 /ws。
    kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null
  done
  sleep 0.3
  for pid in "${SSE_PID:-}" "${NODE_PID:-}" "${HUB_PID:-}"; do
    [ -n "$pid" ] || continue
    kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null
  done
  # `set -u` 下若 TMP 还没赋值（脚本在 mkdir 之前就失败、trap 已生效），
  # `"$TMP"` 会让 cleanup 自己再抛一次 unbound variable，掩盖真正的错。
  if [ "$KEEP" -eq 1 ]; then
    echo "  现场保留在：${TMP:-<未创建>}"
  else
    rm -rf "${TMP:-}"
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
echo "== 0 先决条件 =="
missing=""
for tool in curl jq ss sqlite3 awk sed sh; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
  echo "  ❌ 缺工具：$missing" >&2
  exit 2
fi
echo "  ✅ 工具齐备（curl / jq / ss / sqlite3 / sh）"

# 终端 connector 要 spawn 一个 shell。缺了它整条链路都建不起来，而这与产品
# 行为无关——所以是环境问题（退出码 2），不是断言失败。
if ! command -v "${SHELL:-/bin/sh}" >/dev/null 2>&1; then
  echo "  ❌ 找不到可用的 shell（SHELL=${SHELL:-未设}）：终端 connector 起不来" >&2
  exit 2
fi

AFHUB_BIN=${AFHUB_BIN:-$ROOT/target/debug/afhub}
AFCLI_BIN=${AFCLI_BIN:-$ROOT/target/debug/afcli}
if [ ! -x "$AFHUB_BIN" ] || [ ! -x "$AFCLI_BIN" ]; then
  echo "  二进制不存在，先构建（CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0）…"
  # 磁盘：这台机器没有 swap，并发编译会撞 OOM。`CARGO_BUILD_JOBS=1` 是硬要求。
  if ! CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 cargo build --quiet \
    --bin afhub --bin afcli; then
    echo "  ❌ cargo build 失败（若返回 101 且无输出，先看磁盘是否满）" >&2
    exit 2
  fi
fi
if [ ! -x "$AFHUB_BIN" ] || [ ! -x "$AFCLI_BIN" ]; then
  echo "  ❌ 构建后仍找不到 $AFHUB_BIN / $AFCLI_BIN" >&2
  exit 2
fi
echo "  ✅ 验收对象：$AFHUB_BIN / $AFCLI_BIN"

# ---------------------------------------------------------------------------
# 现场：临时目录 + 一个普通工作区（本脚本不关心 git 内容，只关心通道生死）
#
# 工作区只需要「存在」：terminal connector 的 `cwd` 落在它里面，用来证明
# `open` 真的到了设备、真的 spawn 了一个 shell。不需要是 git 仓库——git 面板
# 的语义归 v0.4.0 的脚本管，这里再验一遍只是重复。
# ---------------------------------------------------------------------------
TMP_BASE=${TMPDIR:-/tmp}
mkdir -p "$TMP_BASE" 2>/dev/null || TMP_BASE=/tmp
TMP=$(mktemp -d "$TMP_BASE/agentfleet-e2e-v050.XXXXXX") || {
  echo "  ❌ 无法创建临时目录（TMPDIR=$TMP_BASE）" >&2
  exit 2
}
mkdir -p "$TMP/hub" "$TMP/node" "$TMP/ws"
WS=$TMP/ws

HUB_LOG=$TMP/hub.log
SSE_LOG=$TMP/sse.log
COOKIES=$TMP/cookies.txt
BODY=$TMP/last-body.json
NODE_LOG_DIR=$TMP/node/agentfleet
# Hub 的库文件。断言 1 要读 `device.capabilities`（`/api/devices` 的对外契约里
# **没有** connectors 字段）。路径定一处：散在各断言里各拼一次，改目录名时
# 会漏改一处而静默读空。
HUB_DB=$TMP/hub/agentfleet.db

if [ -z "$PORT" ]; then
  for _ in $(seq 1 20); do
    PORT=$(((RANDOM % 2000) + 18000))
    ss -ltn 2>/dev/null | grep -q ":$PORT " || break
  done
fi
BASE="http://127.0.0.1:$PORT"
echo "  现场：$TMP（Hub 端口 $PORT）"

# ---------------------------------------------------------------------------
echo "== 起真 afhub =="
AFHUB_DATA_DIR=$TMP/hub AFHUB_ADDR="127.0.0.1:$PORT" "$AFHUB_BIN" >>"$HUB_LOG" 2>&1 &
HUB_PID=$!
for _ in $(seq 1 100); do
  curl -sS -o /dev/null "$BASE/api/health" 2>/dev/null && break
  sleep 0.1
done
if ! curl -sS -o /dev/null "$BASE/api/health" 2>/dev/null; then
  echo "  ❌ Hub 10s 内没有起来" >&2
  tail -20 "$HUB_LOG" >&2
  exit 1
fi
SETUP_TOKEN=$(sed 's/\x1b\[[0-9;]*m//g' "$HUB_LOG" | awk '/setup token/ { print $NF; exit }')
if [ -z "$SETUP_TOKEN" ]; then
  echo "  ❌ Hub 日志里没有 setup token" >&2
  sed 's/\x1b\[[0-9;]*m//g' "$HUB_LOG" | tail -20 >&2
  exit 1
fi
PW='e2e-v050-passphrase'
SETUP_CODE=$(curl -sS -o "$BODY" -w '%{http_code}' -X POST "$BASE/setup" \
  -H "x-setup-token: $SETUP_TOKEN" -H 'content-type: application/json' \
  --data-binary "{\"password\":\"$PW\"}")
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
  echo "  ❌ cookie jar 里没有 af_csrf：后续每个 POST/DELETE 都会 403" >&2
  exit 1
fi
ok "setup + login，拿到 af_csrf"

# 认证口径只此一处：cookie jar 与 CSRF 头名都从这里出。
curl_auth() {
  curl -sS -b "$COOKIES" -H "x-csrf-token: $CSRF" "$@"
}

: >"$SSE_LOG"
curl_auth -N "$BASE/api/stream" >>"$SSE_LOG" 2>&1 &
SSE_PID=$!

# `METHOD PATH [BODY]` → HTTP_CODE / LAST_BODY 两个全局量。**不打印 stdout**：
# 别在 `$(...)` 里读 LAST_BODY——赋值发生在子 shell 里，父 shell 留着上一次的残留。
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

# 某条通道的所有 `channel.frame` 载荷（JSON 行）。
frames_of() {
  awk -v want="channel.frame" '
    /^event: / { ev = substr($0, 7); gsub(/^ +/, "", ev) }
    /^data: /  { if (ev == want) { d = substr($0, 6); sub(/^ /, "", d); print d } }
  ' "$SSE_LOG" | jq -c --arg c "$1" 'select(.channel_id == $c) | .payload' 2>/dev/null
}

wait_until() {
  local deadline=$((SECONDS + $1))
  shift
  while :; do
    if "$@"; then return 0; fi
    [ "$SECONDS" -ge "$deadline" ] && return 1
    sleep 0.2
  done
}

dump() {
  local what=$1
  echo "     —— 现场（$what）——"
  echo "     Hub 日志里与通道/错误有关的行（末 10 行）："
  sed 's/\x1b\[[0-9;]*m//g' "$HUB_LOG" | grep -E 'channel|WARN|ERROR' | tail -10 | sed 's/^/       /'
  if [ -f "$NODE_LOG_DIR/logs/afcli.log" ]; then
    echo "     设备日志（末 10 行）："
    sed 's/\x1b\[[0-9;]*m//g' "$NODE_LOG_DIR/logs/afcli.log" | tail -10 | sed 's/^/       /'
  fi
  echo "     SSE 里这条通道收到过的载荷："
  frames_of "${CH:-}" | head -10 | sed 's/^/       /'
}

# ---------------------------------------------------------------------------
echo
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
# 1 设备如实上报了 terminal connector（通道门禁的前提）
#
# **从 `device.capabilities` 读，不从 `/api/devices` 读**：那个端点的对外契约
# 只有 `{id,name,platform,node_version,status,last_seen,cpu,mem,gpu,agents,
# usable_agents,derived,key_fingerprint}`——**没有 connectors**。按
# `.[0].capabilities.connectors` 取会拿到 null，`jq` 静默给 `null` 且退出码 0，
# 于是断言永远红，而红的原因看起来像「设备没上报 terminal」。
# （M2 与 v0.4.0 各踩过一次同一坑，见 `scripts/e2e-v0.5.0/README.md` §3。）
# ---------------------------------------------------------------------------
echo
echo "== 1 心跳上报 connectors 含 terminal =="
connectors_ready() {
  local got
  got=$(sqlite3 "$HUB_DB" \
    "select json_extract(capabilities, '$.connectors') from device where id='$NODE_ID';" 2>/dev/null)
  printf '%s' "$got" | grep -q '"terminal"'
}
if wait_until 30 connectors_ready; then
  GOT_CONN=$(sqlite3 "$HUB_DB" \
    "select json_extract(capabilities, '$.connectors') from device where id='$NODE_ID';" 2>/dev/null)
  ok "设备上报 connectors=$GOT_CONN（含 terminal）"
else
  bad "30s 内 device.capabilities 的 connectors 里没有 terminal——开通道会 409"
  info "  实际存的值：$(sqlite3 "$HUB_DB" \
    "select json_extract(capabilities, '$.connectors') from device where id='$NODE_ID';" 2>/dev/null)"
  dump "断言 1"
fi

# ---------------------------------------------------------------------------
# 2 建会话
# ---------------------------------------------------------------------------
echo
echo "== 2 建会话 =="
SID=""
AID=$(sqlite3 "$HUB_DB" \
  "select agent_id from agent_runtime where node_id='$NODE_ID' and available=1 limit 1;" 2>/dev/null)
if [ -z "$AID" ]; then
  AID=$(sqlite3 "$HUB_DB" \
    "select agent_id from agent_runtime where node_id='$NODE_ID' limit 1;" 2>/dev/null)
fi
if [ -z "$AID" ]; then
  echo "  ❌ 设备上一个 agent 都没上报：会话建不起来（这不是 terminal 的问题）" >&2
  dump "断言 2"
  exit 2
fi
info "会话带的 agent_id=$AID（与 terminal 无关，但会话这一层必填）"
api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS\",\"agent_id\":\"$AID\"}"
if [ "$HTTP_CODE" = "201" ]; then
  SID=$(printf '%s' "$LAST_BODY" | jq -r '.id // empty')
  ok "建会话 201，session=$SID（工作区 $WS）"
else
  bad "建会话回 $HTTP_CODE（期望 201）：$LAST_BODY"
  dump "断言 2"
fi

# ---------------------------------------------------------------------------
# 辅助：开一条 terminal 通道，回 0/1，通道号写进 OPENED。
#
# 页面在「选中终端」那一下开通道（§5.5.1 的懒建立）。本脚本按同一条路走：
# `POST /api/channels`。参数给 `cwd`，好证明它真的到了设备、真的 spawn 了 shell。
# ---------------------------------------------------------------------------
OPENED=""
open_terminal() {
  local body code
  OPENED=""
  body="{\"session_id\":\"$SID\",\"connector\":\"terminal\",\"params\":{\"cwd\":\"$WS\",\"cols\":80,\"rows\":24}}"
  code=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    -H 'content-type: application/json' "$BASE/api/channels" --data-binary "$body")
  if [ "$code" = "201" ]; then
    OPENED=$(jq -r '.channel_id // empty' "$BODY" 2>/dev/null)
    [ -n "$OPENED" ]
  else
    info "开 terminal 通道回 $code：$(cat "$BODY")"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# 3 选中终端 = 开它自己那一条通道，且设备真的 spawn 了 shell
# ---------------------------------------------------------------------------
echo
echo "== 3 选中终端 → 开通道 + 设备侧真 spawn =="
CH=""
if [ -n "$SID" ]; then
  if open_terminal; then
    CH=$OPENED
    ok "开 terminal 通道 201，channel=$CH"
  else
    bad "开 terminal 通道失败（期望 201）"
    dump "断言 3 开通道"
  fi
else
  bad "断言 2 没拿到会话号，断言 3 无法开始"
fi

# 设备侧真起了 shell 的证据：Hub 收到了 `channel.ready`。
# 这一条不能靠「POST 回 201」代替——201 只说明 Hub 记了账，`ready` 才说明
# 设备把 PTY 建起来了。
ready_seen() {
  frames_of "$CH" >/dev/null
  awk -v want="channel.ready" '
    /^event: / { ev = substr($0, 7); gsub(/^ +/, "", ev) }
    /^data: /  { if (ev == want) { d = substr($0, 6); sub(/^ /, "", d); print d } }
  ' "$SSE_LOG" | jq -e --arg c "$CH" 'select(.channel_id == $c)' >/dev/null 2>&1
}
if [ -n "$CH" ]; then
  if wait_until 20 ready_seen; then
    ok "设备侧回了 channel.ready（PTY 真的建起来了，不是只有 Hub 记账）"
  else
    bad "20s 内没有 channel.ready：设备没有真的把终端建起来"
    dump "断言 3 ready"
  fi
fi

# ---------------------------------------------------------------------------
# 4 换工具 = 替换 + 释放被换掉那一条（§7.1.3 的核心断言）
#
# 页面在 `openPanel` 里对「上一个工具」发 `DELETE /api/channels/{id}`。本脚本
# 按同一条路走，然后在**两个层次**上验它真的发生了：
#   (a) Hub 回 204；
#   (b) 旧通道**再也发不进去**——只验 (a) 的话，一个「回了 204 但没通知设备」
#       的实现也能过，而那样 PTY 会活着，用户丢掉的 shell 就白丢了。
# ---------------------------------------------------------------------------
echo
echo "== 4 换工具 → 旧通道 204，且旧通道再也发不进去 =="
CH2=""
if [ -n "$CH" ]; then
  api DELETE "/api/channels/$CH"
  if [ "$HTTP_CODE" = "204" ]; then
    ok "释放旧通道 $CH → 204"
  else
    bad "释放旧通道回 $HTTP_CODE（期望 204）：$LAST_BODY"
    dump "断言 4 释放"
  fi

  # (b) 旧通道必须不再可用。判据取「发不进去」而不是「Hub 表里还有没有」：
  # 后者是内部状态，前者才是页面/设备看到的事实。**任何非 2xx 都算数**
  # （404 通道不存在、409 已关……），因为它们都意味着「这条路断了」。
  code=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    "$BASE/api/channels/$CH/send" --data-binary '{"bytes":"lA=="}')
  case "$code" in
    2??)
      bad "旧通道 $CH 已释放，却仍能 send（回 $code）——设备侧的 PTY 不会关"
      info "  载荷：$(cat "$BODY")"
      dump "断言 4 旧通道仍可用"
      ;;
    *)
      ok "旧通道 $CH 已不可发送（回 $code：这条路真的断了）"
      ;;
  esac

  # 换上去的新工具（这里用第二条 terminal 通道代表「换成另一个工具」）。
  # 用 terminal 而不是 git：terminal 是**唯一**有真实资源代价的工具
  # （git 通道释放不杀任何东西），所以用它才验得到「旧工具真的没了」。
  if open_terminal; then
    CH2=$OPENED
    ok "换上去的工具开了新通道 $CH2（与旧通道并存不矛盾：旧的那条已经断了）"
  else
    bad "换上去的工具开通道失败"
    dump "断言 4 新通道"
  fi
else
  bad "断言 3 没拿到通道，断言 4 无法开始"
fi

# ---------------------------------------------------------------------------
# 5 「＋」选回当前那一个 = 零副作用（§7.1.2 的唯一例外）
#
# 页面的结构保证是 `PanelDock.addPanel` 在组件内判等返回、**不 emit**，
# 所以信号压根送不到释放通道那一侧。脚本这一侧能观察到的是「这条通道
# 的生死一点没变」：
#   (a) 它仍然活着（能收），且
#   (b) 它的身份没被换掉——页面若走了「先释放再重建」，会得到一条**新**的
#       channel_id，而新 id 一定不等于手里这个。
#
# **不要用 `GET /api/channels` 数通道**：那个端点在 Hub 上根本不存在
# （`crates/hub-server/src/lib.rs:677` 只注册了 `POST /api/channels`）。
# 按它取会拿到 405，而 `jq` 对空体静默给 null，于是断言永远红、红的原因
# 看起来像「通道数不对」。（README §3 记的第二个坑正是这一类。）
# ---------------------------------------------------------------------------
echo
echo "== 5 选回当前那个工具 → 零副作用（不重建、不释放）=="
if [ -n "$CH2" ]; then
  # (b) 身份：**在检查前后比对 SSE 日志**，而不是「日志里存在这条通道的 ready」。
  #
  # 后者是这套脚本里最容易写成恒真的一条：`ready` 是**已经发生过**的事件，
  # 它躺在日志里不会消失——于是「先释放再重建」之后旧 id 的那一行还在，
  # 判据照样命中，断言恒绿。（实测：构造「点当前那个 = 先关再开」的变异，
  # 这种写法依然 18/18 全绿。所以改成看**增量**。）
  #
  # 页面「点当前那个」不产生任何请求，所以这一整段之后：不该有新的
  # `channel.ready`，也不该有任何别的通道 id 冒出来。
  ready_lines() {
    awk -v want="channel.ready" '
      /^event: / { ev = substr($0, 7); gsub(/^ +/, "", ev) }
      /^data: /  { if (ev == want) { d = substr($0, 6); sub(/^ /, "", d); print d } }
    ' "$SSE_LOG" 2>/dev/null
  }
  READY_BEFORE=$(ready_lines | wc -l | tr -d ' ')
  # 「增量相等」这条判据有一个空洞：**基线为 0 时它恒真**。若前面的开通道
  # 步骤其实没跑成（SSE_LOG 空、或 `channel.ready` 根本没到），0 → 0 会
  # 伪装成「没有重建」。所以先把基线钉住：它必须 ≥1。
  # 反向验证：把上面那条 ready_lines 的 `want` 改成不存在的名字 → 这条红。
  if [ "$READY_BEFORE" -gt 0 ]; then
    info "断言5 基线：已有 ${READY_BEFORE} 条 channel.ready（非 0，增量判据才有效）"
  else
    bad "断言5 基线为 0：读不到任何 channel.ready，「增量相等」会退化成恒真（恒绿），这条断言不可信"
    dump "断言 5 基线"
  fi
  # 给页面足够的时间去做出「不该做」的事（真做了的话，Pod 侧那一秒内就会发请求）。
  sleep 1
  READY_AFTER=$(ready_lines | wc -l | tr -d ' ')
  if [ "$READY_BEFORE" = "$READY_AFTER" ]; then
    ok "点当前那个工具之后没有新的 channel.ready（${READY_BEFORE} → ${READY_AFTER}）：没有重建"
  else
    bad "点当前那个工具之后多出了新的 channel.ready（${READY_BEFORE} → ${READY_AFTER}）：它走了「先释放再重建」"
    ready_lines | tail -3 | sed 's/^/       /'
    dump "断言 5 身份"
  fi

  # (a) 生死：仍然能发进去。这是「零副作用」最直接的可观察形式。
  code=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    "$BASE/api/channels/$CH2/send" --data-binary '{"bytes":"bHMK\n"}')
  case "$code" in
    2??) ok "当前通道 $CH2 仍可发送（$code）：它没有被杀掉" ;;
    *)
      bad "当前通道 $CH2 已不可发送（回 $code）——「点当前那个」不该杀掉它"
      info "  载荷：$(cat "$BODY")"
      dump "断言 5 当前通道"
      ;;
  esac
else
  bad "断言 4 没拿到新通道，断言 5 无法开始"
fi

# ---------------------------------------------------------------------------
# 6 每次替换**只**释放被换掉那一个（不是「把全部清空」）
#
# 页面的 `openPanel` 只看 `ws.panel` 一个前值。把它错写成「关掉所有通道」
# 在单通道测试里看不出来（本来也只有一条），但用户会因此丢掉本会话里
# 别的工具的通道。这里先开第二条，再替换第一条，验第二条还活着。
# ---------------------------------------------------------------------------
echo
echo "== 6 替换只释放被换掉那一个，不牵连其它通道 =="
if [ -n "$CH2" ]; then
  OTHER_BEFORE=""
  if open_terminal; then
    OTHER_BEFORE=$OPENED
    ok "再开一条对照通道 $OTHER_BEFORE"
  else
    bad "对照通道开不出来，断言 6 无法开始"
  fi

  if [ -n "$OTHER_BEFORE" ] && [ "$OTHER_BEFORE" != "$CH2" ]; then
    api DELETE "/api/channels/$CH2"
    if [ "$HTTP_CODE" = "204" ]; then
      ok "替换时释放 $CH2 → 204"
    else
      bad "替换时释放 $CH2 回 $HTTP_CODE（期望 204）"
      dump "断言 6 释放"
    fi
    # 对照通道必须还活着。
    code=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
      "$BASE/api/channels/$OTHER_BEFORE/send" --data-binary '{"bytes":"bHMK"}')
    case "$code" in
      2??) ok "对照通道 $OTHER_BEFORE 仍可发送（$code）：替换没有牵连它" ;;
      *)
        bad "对照通道 $OTHER_BEFORE 不可发送（回 $code）——替换把别的通道也关了"
        info "  载荷：$(cat "$BODY")"
        dump "断言 6 对照通道"
        ;;
    esac
    # 反向验证：被释放那条必须真的断了。只验「对照活着」的话，一个
    # 「什么都不释放」的实现也能过。
    code=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
      "$BASE/api/channels/$CH2/send" --data-binary '{"bytes":"bHMK"}')
    case "$code" in
      2??) bad "被释放的 $CH2 仍可发送（回 $code）：替换没杀掉它" ;;
      *) ok "被释放的 $CH2 已不可发送（回 $code）" ;;
    esac
  fi
fi

# ---------------------------------------------------------------------------
# 7 收起右侧一格 → 名下通道全部释放（「退出即释放」）
#
# 页面的 `closeDock` 逐个释放。这里把还活着的那条关掉，验 204 + 再也发不进去。
# ---------------------------------------------------------------------------
echo
echo "== 7 收起右侧一格 → 名下通道全部释放 =="
if [ -n "${OTHER_BEFORE:-}" ]; then
  api DELETE "/api/channels/$OTHER_BEFORE"
  if [ "$HTTP_CODE" = "204" ]; then
    ok "收起时释放 $OTHER_BEFORE → 204"
  else
    bad "收起时释放回 $HTTP_CODE（期望 204）"
    dump "断言 7 释放"
  fi
  code=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    "$BASE/api/channels/$OTHER_BEFORE/send" --data-binary '{"bytes":"bHMK"}')
  case "$code" in
    2??) bad "收起之后 $OTHER_BEFORE 仍可发送（回 $code）" ;;
    *) ok "收起之后 $OTHER_BEFORE 已不可发送（回 $code）" ;;
  esac
  # 幂等：再关一次仍 204。前端「关面板」与「切 tab」会连打两次，
  # 让第二次变成 404 只会逼出一个「先查再关」的往返。
  api DELETE "/api/channels/$OTHER_BEFORE"
  if [ "$HTTP_CODE" = "204" ]; then
    ok "重复释放同一通道仍 204（幂等，前端连打两次不会变成 404）"
  else
    bad "重复释放回 $HTTP_CODE（期望 204 幂等）"
    dump "断言 7 幂等"
  fi
else
  bad "断言 6 没有留下对照通道，断言 7 无法开始"
fi

# ---------------------------------------------------------------------------
echo
echo "== 结果 =="
echo "  通过 $pass，失败 $fail"
if [ "$fail" -ne 0 ]; then
  echo "  ❌ 验收未通过" >&2
  [ "$KEEP" -eq 0 ] && echo "     （加 --keep 重跑可保留现场）" >&2
  exit 1
fi
echo "  ✅ 验收通过"
