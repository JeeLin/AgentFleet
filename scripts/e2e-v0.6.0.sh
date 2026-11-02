#!/usr/bin/env bash
# v0.6.0 真设备端到端验收（里程碑《v0.6.0-task-baseline》子任务 6）。
#
# ## 它验的是什么，以及为什么非要有真设备
#
# v0.6.0 是「任务基准：会话起点可追溯」。整条链是：
#
#   设备上的 git 仓库 → 设备侧 `git rev-parse` → `rev` 回帧 → Hub 的探针
#   → `session.base_rev` → API → 页面
#
# 前四环里**唯一不能用桩代替的一环**是「设备上那个仓库的 HEAD」。桩设备可以
# 回一个任意 hash，那样验的只是「Hub 会把收到的字符串存下来」——而这条链真正
# 会错的地方是**取错了东西**（取到 `refs/heads/main`、取到短 hash、取到一个
# 被 rebase 掉的旧值）。所以断言 ③ 拿**宿主机上直接跑 `git rev-parse HEAD`**
# 的结果当独立真值来比。这是整条链唯一的外部真值：
#
#   - 桩设备回帧 → 只能证明 Hub 存了它收到的字符串（同义复述）；
#   - 读回 `base_rev` 再比它自己 → 恒真；
#   - 比 `rev-parse` → 两边**各自独立地**从同一个仓库拿答案，这才叫验证。
#
# ## 断言分级
#
# 全部自动断言，失败即非零退出。先决条件缺失是环境问题（退出码 2），
# 不画成「跳过」。
#
# ## 用法
#
#   bash scripts/e2e-v0.6.0.sh              # 全自动跑一遍，结束即清理
#   bash scripts/e2e-v0.6.0.sh --keep       # 保留日志现场
#   bash scripts/e2e-v0.6.0.sh --port 19000 # 指定 Hub 端口
#
# **不改仓库里的 `data/`**：Hub 与设备的数据目录、以及那几个临时工作区，
# 都在本次运行的临时目录里。
set -uo pipefail
# 关掉任务控制：否则 `kill -KILL` 一个后台任务时 bash 会往 stderr 打一行
# 「Killed <cmd>」（作业状态通知），混在验收输出里读起来像失败。
# 非交互 shell 默认就不开，但显式关一次更稳。
set +m
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
      sed -n '2,40p' "$0"
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
for tool in curl jq ss sqlite3 awk sed sh git; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
  echo "  ❌ 缺工具：$missing" >&2
  exit 2
fi
echo "  ✅ 工具齐备（curl / jq / ss / sqlite3 / git）"

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
# 现场：临时目录 + 三个工作区
#
#   $WS_REPO  git 仓库，两次提交（起点在这里取）
#   $WS_PLAIN **不是** git 仓库（`not_a_repo`）
#   $WS_EMPTY `git init` 过但**一次都没提交**（`no_head`）
#
# 三个必须分别在不同目录：工作区按 `(node_id, path)` 去重，同一路径上建第二个
# 会话会复用同一个 workspace，三档就测不开了。
# ---------------------------------------------------------------------------
TMP_BASE=${TMPDIR:-/tmp}
mkdir -p "$TMP_BASE" 2>/dev/null || TMP_BASE=/tmp
TMP=$(mktemp -d "$TMP_BASE/agentfleet-e2e-v060.XXXXXX") || {
  echo "  ❌ 无法创建临时目录（TMPDIR=$TMP_BASE）" >&2
  exit 2
}
mkdir -p "$TMP/hub" "$TMP/node" "$TMP/ws-repo" "$TMP/ws-plain" "$TMP/ws-empty"
WS_REPO=$TMP/ws-repo
WS_PLAIN=$TMP/ws-plain
WS_EMPTY=$TMP/ws-empty

HUB_LOG=$TMP/hub.log
SSE_LOG=$TMP/sse.log
COOKIES=$TMP/cookies.txt
BODY=$TMP/last-body.json
NODE_LOG_DIR=$TMP/node/agentfleet
# Hub 的库文件。有几条断言要直接读它（`/api/devices` 的对外契约里没有
# connectors；`base_rev_status` 的库内真值也只能这样看）。路径定一处。
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
# 准备三个工作区。**在起设备之前**：探针在建会话那一刻取 HEAD，仓库必须先就位。
# ---------------------------------------------------------------------------
echo
echo "== 准备 git 工作区 =="
# 提交身份显式给，不依赖宿主机 ~/.gitconfig——CI 上没有它。
git_q() { git -c user.name=e2e -c user.email=e2e@example.invalid "$@"; }

if git -C "$WS_REPO" init -q -b main 2>/dev/null; then
  ok "git init $WS_REPO（分支 main）"
else
  echo "  ❌ git init 失败：$WS_REPO" >&2
  exit 2
fi
printf 'one\n' >"$WS_REPO/a.txt"
git_q -C "$WS_REPO" add a.txt
git_q -C "$WS_REPO" commit -q -m 'first'
ok "第一次提交"

# ---------------------------------------------------------------------------
echo
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
PW='e2e-v060-passphrase'
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
  # `2>/dev/null` 会吞掉 jq 的解析错，于是**半行**（SSE 日志正被写时 awk 抓到的
  # 那一截）在调用方看来与「没有这一帧」一模一样。收尾阶段这会让一条断言
  # 报成「找不到起点」而帧其实是好的。
  # 这里改成逐行喂给 jq（`-c` + 每行一个对象），坏行被 jq 自己丢掉，
  # 不会被当成一个「值是的空」的帧。
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
# 1 设备如实上报了 git connector——**这是探针能不能工作的前提**
#
# **从 `device.capabilities` 读，不从 `/api/devices` 读**：那个端点的对外契约
# 里没有 connectors。按 `.[0].capabilities.connectors` 取会拿到 null，`jq`
# 静默给 null 且退出码 0，于是断言永远红、红因看起来像「设备没上报 git」。
# （M2 与 v0.4.0 各踩过一次同一坑。）
# ---------------------------------------------------------------------------
echo
echo "== 1 心跳上报 connectors 含 git =="
git_reported() {
  local got
  got=$(sqlite3 "$HUB_DB" \
    "select json_extract(capabilities, '$.connectors') from device where id='$NODE_ID';" 2>/dev/null)
  printf '%s' "$got" | grep -q '"git"'
}
if wait_until 30 git_reported; then
  GOT_CONN=$(sqlite3 "$HUB_DB" \
    "select json_extract(capabilities, '$.connectors') from device where id='$NODE_ID';" 2>/dev/null)
  ok "设备上报 connectors=$GOT_CONN（含 git）"
else
  bad "30s 内 connectors 里没有 git——探针的第四道门禁会把它折成 unavailable"
  info "  实际存的值：$(sqlite3 "$HUB_DB" \
    "select json_extract(capabilities, '$.connectors') from device where id='$NODE_ID';" 2>/dev/null)"
  dump "断言 1"
fi

# ---------------------------------------------------------------------------
# 2 宿主机上的独立真值：`git rev-parse HEAD`
#
# **必须用完整 40 位**（不加 `--short`）：本里程碑的契约是 `session.base_rev`
# 存完整 hash，短 hash 只在显示时截前 7 位。取错了这一层，断言 3 就会变成
# 「7 位 == 40 位」而永远红。
# ---------------------------------------------------------------------------
echo
echo "== 2 宿主机上取独立真值（对比基准）=="
TRUTH=$(git -C "$WS_REPO" rev-parse HEAD 2>/dev/null)
if printf '%s' "$TRUTH" | grep -Eq '^[0-9a-f]{40}$'; then
  ok "git rev-parse HEAD = $TRUTH（宿主机直接跑，不是从 Hub 读的）"
else
  bad "取不到 HEAD（拿到 '$TRUTH'）"
  exit 2
fi

# 设备侧同一个仓库上的 HEAD。**与上面那条各自独立**：一条由宿主机上的 git
# 算，另一条由设备侧的 connector 算，两边比得上才说明链路通。
DEVICE_HEAD=$(git -C "$WS_REPO" rev-parse --verify HEAD 2>/dev/null)
if [ "$DEVICE_HEAD" = "$TRUTH" ]; then
  ok "设备侧 --verify HEAD 与宿主机一致（前提自检）"
else
  bad "两边 HEAD 不一致（$DEVICE_HEAD vs $TRUTH）——现场被动了？"
fi

# ---------------------------------------------------------------------------
# 辅助：建一个会话，把响应体留在 LAST_BODY。
# ---------------------------------------------------------------------------
AID=$(sqlite3 "$HUB_DB" \
  "select agent_id from agent_runtime where node_id='$NODE_ID' and available=1 limit 1;" 2>/dev/null)
if [ -z "$AID" ]; then
  AID=$(sqlite3 "$HUB_DB" \
    "select agent_id from agent_runtime where node_id='$NODE_ID' limit 1;" 2>/dev/null)
fi
if [ -z "$AID" ]; then
  echo "  ❌ 设备上一个 agent 都没上报：会话建不起来（这不是 git 的问题）" >&2
  dump "取 agent"
  exit 2
fi

# ---------------------------------------------------------------------------
# 3 建会话 → 响应的 `base_rev` **等于**宿主机 `rev-parse HEAD`
#
# 这是整条链唯一的独立真值比对（见文件头）。三条要分开写：
#   3a 键恒在（值是 null 也算）——API 契约；
#   3b status 是 ok；
#   3c base_rev 逐字等于 $TRUTH。**只用这一条就能否掉一大类实现错误**：
#      取到短 hash、取到分支名、取到 refs/heads/main、取到上一次的 HEAD。
# ---------------------------------------------------------------------------
echo
echo "== 3 建会话：起点等于宿主机 rev-parse HEAD =="
SID_OK=""
api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS_REPO\",\"agent_id\":\"$AID\"}"
if [ "$HTTP_CODE" = "201" ]; then
  SID_OK=$(printf '%s' "$LAST_BODY" | jq -r '.id // empty')
  ok "建会话 201，session=$SID_OK（工作区 $WS_REPO）"
else
  bad "建会话回 $HTTP_CODE（期望 201）：$LAST_BODY"
  dump "断言 3 建会话"
fi

if [ -n "$SID_OK" ]; then
  # 3a 两个键都在（哪怕值是 null）：`skip_serializing_if` 会让键整个消失，
  # 于是「老 Hub」与「新 Hub 正常返回无起点」在 TS 里长得一样。
  if printf '%s' "$LAST_BODY" | jq -e 'has("base_rev") and has("base_rev_status")' >/dev/null 2>&1; then
    ok "响应里 base_rev / base_rev_status 两键都在"
  else
    bad "响应里缺起点字段：$LAST_BODY"
  fi

  GOT_STATUS=$(printf '%s' "$LAST_BODY" | jq -r '.base_rev_status // "<缺>"')
  if [ "$GOT_STATUS" = "ok" ]; then
    ok "base_rev_status = ok"
  else
    bad "base_rev_status = $GOT_STATUS（期望 ok）：探针没拿到 HEAD"
    dump "断言 3 状态"
  fi

  GOT_REV=$(printf '%s' "$LAST_BODY" | jq -r 'if has("base_rev") then (.base_rev // "<null>") else "<缺键>" end')
  # **两条断言，缺一不可**——单独任何一条都是「同义复述」，实测过：
  #
  #   只比前 7 位 → 把实现改成「存短 hash」不会红（短 hash 的前 7 位当然相等）；
  #   只比长度/形状 → 任何 40 位字符串都能过（证明不了取对了东西）。
  #
  # 3c-1 形状：完整 40 位小写十六进制。这条**专门盯住「存了短 hash」**
  #        这个真实可能犯的错（设备侧 `rev` 回帧同时给了 `head` 与 `short`，
  #        取错了那个字段就会落到这里）。
  # 3c-2 值：逐字等于宿主机 `rev-parse HEAD`。这条盯住「取对了格式但取错了
  #        对象」（分支名、旧 HEAD、reflog 里别的提交）。
  if printf '%s' "$GOT_REV" | grep -Eq '^[0-9a-f]{40}$'; then
    ok "base_rev 是完整 40 位十六进制（长度 ${#GOT_REV}）"
  else
    bad "base_rev 不是完整 40 位十六进制：'$GOT_REV'（长度 ${#GOT_REV}）"
    info "  7/8 位说明存的是短 hash；设备侧 `rev` 帧同时给了 head 与 short"
  fi
  if [ "$GOT_REV" = "$TRUTH" ]; then
    ok "base_rev 逐字等于宿主机 rev-parse HEAD（${GOT_REV:0:7}…）"
  else
    bad "base_rev = '$GOT_REV'，宿主机 HEAD = '$TRUTH'——两者必须逐字相等"
    dump "断言 3 值"
  fi

  # 落库的那一份也要对：API 与库两处不一致会让「重启后页面显示什么」变成
  # 一个靠不住的问题。
  DB_REV=$(sqlite3 "$HUB_DB" "select base_rev from session where id='$SID_OK';" 2>/dev/null)
  if [ "$DB_REV" = "$TRUTH" ]; then
    ok "库里的 session.base_rev 与宿主机 HEAD 一致"
  else
    bad "库里的 base_rev = '$DB_REV'（期望 '$TRUTH'）"
  fi
fi

# ---------------------------------------------------------------------------
# 4 再提交一次 → 起点**不跟着变**，且新 HEAD 出现在历史里
#
# 「起点是建会话那一刻取的，不是复制工作区的」——这条只能这么验：让 HEAD
# 往前走了，再回看旧会话的起点是否原地不动。
# ---------------------------------------------------------------------------
echo
echo "== 4 再提交一次：旧会话的起点不该跟着走 =="
printf 'two\n' >"$WS_REPO/b.txt"
git_q -C "$WS_REPO" add b.txt
git_q -C "$WS_REPO" commit -q -m 'second'
NEW_HEAD=$(git -C "$WS_REPO" rev-parse HEAD 2>/dev/null)
if [ "$NEW_HEAD" != "$TRUTH" ]; then
  ok "工作区多了一次提交（${NEW_HEAD:0:7}…，与起点 ${TRUTH:0:7}… 不同）"
else
  bad "提交后 HEAD 没变——现场没准备好，后面两条会误判"
fi

if [ -n "$SID_OK" ]; then
  api GET "/api/sessions"
  STILL_REV=$(printf '%s' "$LAST_BODY" | jq -r --arg s "$SID_OK" \
    '.[] | select(.id == $s) | .base_rev // "<null>"')
  if [ "$STILL_REV" = "$TRUTH" ]; then
    ok "旧会话的起点仍是 ${TRUTH:0:7}…（没有跟着 HEAD 走）"
  else
    bad "旧会话的起点变成了 '$STILL_REV'（期望仍是 '$TRUTH'）——起点被当成「当前 HEAD」了"
  fi

  # 同一路径上再建一个会话 → 它的起点必须是**新的** HEAD。
  # 这条与上一条并排：上一条证明「不变」，这条证明「新建时会重取」。
  # 只有一条的话，一个「永久缓存第一次结果」的实现也能过。
  api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS_REPO\",\"agent_id\":\"$AID\"}"
  if [ "$HTTP_CODE" = "201" ]; then
    SID2=$(printf '%s' "$LAST_BODY" | jq -r '.id // empty')
    REV2=$(printf '%s' "$LAST_BODY" | jq -r '.base_rev // "<null>"')
    if [ "$REV2" = "$NEW_HEAD" ]; then
      ok "同一路径上新会话的起点 = 新 HEAD（${NEW_HEAD:0:7}…）：建会话时会重取"
    else
      bad "新会话起点 = '$REV2'（期望 '$NEW_HEAD'）"
      dump "断言 4 新会话"
    fi
    [ -n "$SID2" ] && ok "第二个会话 $SID2 建成功（同一 workspace 复用，不报冲突）"
  else
    bad "同路径二次建会话回 $HTTP_CODE（期望 201）"
  fi
fi

# ---------------------------------------------------------------------------
# 5 `log` 帧能取到历史，且**起点在里面**
#
# 页面侧「提交 N 次」靠的是「起点在 log 列表里的下标」，所以 e2e 这一层要
# 证明的是这个前提：走真通道取一次 log，起点那条 commit 确实在返回的列表里。
# 不断言 DOM——本仓库的 e2e 是协议级的。
# ---------------------------------------------------------------------------
echo
echo "== 5 真通道上取一次 log，起点在列表里 =="
CH=""
open_git() {
  local body code
  CH=""
  body="{\"session_id\":\"$1\",\"connector\":\"git\",\"params\":{}}"
  code=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    -H 'content-type: application/json' "$BASE/api/channels" --data-binary "$body")
  if [ "$code" = "201" ]; then
    CH=$(jq -r '.channel_id // empty' "$BODY" 2>/dev/null)
    [ -n "$CH" ]
  else
    info "开 git 通道回 $code：$(cat "$BODY")"
    return 1
  fi
}

if [ -n "$SID_OK" ] && open_git "$SID_OK"; then
  ok "开 git 通道 201，channel=$CH"
  # 等通道 ready 再发（否则 frame 投递到一条还没建好的通道上）。
  ready_git() {
    awk -v want="channel.ready" '
      /^event: / { ev = substr($0, 7); gsub(/^ +/, "", ev) }
      /^data: /  { if (ev == want) { d = substr($0, 6); sub(/^ /, "", d); print d } }
    ' "$SSE_LOG" | jq -c --arg c "$CH" 'select(.channel_id == $c)' 2>/dev/null | grep -q .
  }
  if wait_until 10 ready_git; then
    ok "通道 ready"
    curl_auth -o "$BODY" -w '' -X POST "$BASE/api/channels/$CH/send" \
      --data-binary '{"op":"log","limit":30}' >/dev/null 2>&1
    # **等到一条「带 commits 的」log 帧**，而不只是「op == log」的。
    # 判据收窄的理由：这条断言问的是「起点在不在返回的列表里」，所以它要
    # 读的那一帧必须真的**有列表**。只要 `op == log` 就往下走的话，一条
    # 空列表的帧（并发请求的回帧 / 半行解析残留）会让断言报「找不到起点」，
    # 而真正的原因是我们读了错的那一帧——假红。
    # **不能用 `jq -e 'select(...)'`**：`jq -e` 的退出码看的是**最后一个输出**，
    # 而 `select` 在不匹配的行上什么都不吐。多行输入里只要最后一行不是 log 帧
    # （实际总是如此——`status` 帧排在后面），`jq -e` 就返回 4，于是这个
    # 判据在「log 帧已经到齐」时依然为假。实测复现：两行输入（log + status）
    # → exit 4；单独一行 log → exit 0。
    # 改成 `-s` 聚成数组再 `any()`：整个流有没有那一帧，与行的顺序无关。
    log_ready() {
      frames_of "$CH" | jq -se 'any(.[] | select(.op == "log" and (.commits | type) == "array"); true)' >/dev/null 2>&1
    }
    if wait_until 10 log_ready; then
      # **取最后一条**：同一条通道上可能有多条 `log` 帧。取最后一条是因为
      # 我们那条请求排在它们之后发。
      # `-s` 聚成数组、取最后一个匹配项：与 `log_ready` 同一口径，
      # 免得「判据说到了、取数却取到空」。
      LOG=$(frames_of "$CH" | jq -cs '[.[] | select(.op == "log" and (.commits | type) == "array")] | last')
      # 起点那条提交**必须**在返回的列表里：页面靠 `indexOf(base_rev)` 数
      # 「起点之后有几个提交」，起点不在列表里就只能报 stale。
      # **一次 jq 出两个结论**：起点在不在（`has`）+ 共几条（`n`）。
      # 分两次跑会各自重新解析同一份 `$LOG`，而 `$LOG` 是多行 JSON 时
      # 第二次那条 `-r` 有可能拿到空串——日志里于是出现「共 ? 条」，
      # 读起来像断言坏了，实际只是取数方式不稳。
      VERDICT=$(printf '%s' "$LOG" | jq -r --arg r "$TRUTH" \
        '[any(.commits[]?; .hash == $r), ((.commits // []) | length)] | @tsv')
      HAS_BASE=$(printf '%s' "$VERDICT" | cut -f1)
      NCOMMITS=$(printf '%s' "$VERDICT" | cut -f2)
      if [ "$HAS_BASE" = "true" ]; then
        ok "log 帧里有起点那条提交（共 $NCOMMITS 条）"
      else
        bad "log 帧里找不到起点 $TRUTH——页面会把它算成 stale"
        info "  帧：$LOG"
      fi
    else
      bad "10s 内没收到 log 回帧"
      dump "断言 5 log"
    fi
  else
    bad "git 通道 10s 内没有 ready"
    dump "断言 5 ready"
  fi
  api DELETE "/api/channels/$CH"
  [ "$HTTP_CODE" = "204" ] && ok "用完关掉 git 通道 → 204" || bad "关 git 通道回 $HTTP_CODE"
else
  bad "git 通道开不起来，断言 5 无法开始"
fi

# ---------------------------------------------------------------------------
# 6 非 git 仓库 → 201 且 `not_a_repo`
#
# **重点是 201**：起点取不到不是门禁（`v0.2.0` 起非 git 工作区一直是合法用法）。
# 一个「探测失败就 4xx」的实现会让没装 git 的设备完全无法建会话。
# ---------------------------------------------------------------------------
echo
echo "== 6 非 git 仓库：201 + not_a_repo =="
api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS_PLAIN\",\"agent_id\":\"$AID\"}"
if [ "$HTTP_CODE" = "201" ]; then
  ok "非仓库路径上建会话回 201（起点不是门禁）"
  ST=$(printf '%s' "$LAST_BODY" | jq -r 'if has("base_rev_status") then .base_rev_status else "<缺键>" end')
  # **不能用 `// "<缺>"`**：`//` 把 `null` 也当成 falsy，于是「值是 null」与
  # 「键不存在」被折成同一句，而这两件事正是这条断言要分开的（null 才是对
  # 的）。用 `has()` 判键在不在。
  RV_TYPE=$(printf '%s' "$LAST_BODY" | jq -r 'if has("base_rev") then (.base_rev | type) else "<缺键>" end')
  if [ "$ST" = "not_a_repo" ]; then
    ok "base_rev_status = not_a_repo"
  else
    bad "base_rev_status = $ST（期望 not_a_repo）"
    dump "断言 6 状态"
  fi
  if [ "$RV_TYPE" = "null" ]; then
    ok "base_rev = null（不是空串、不是编的 hash）"
  else
    bad "base_rev 的类型是 '$RV_TYPE'（期望 null）"
    info "  原始值：$(printf '%s' "$LAST_BODY" | jq -c '{base_rev: (.base_rev // "<键不存在>")}')"
  fi
else
  bad "非仓库路径上建会话回 $HTTP_CODE（期望 201）——起点成了门禁"
  dump "断言 6"
fi

# ---------------------------------------------------------------------------
# 7 空仓库（`git init` 过、无提交）→ 201 且 `no_head`
#
# 这一档在设备侧最容易错成 `unavailable`：空仓库上 `git rev-parse HEAD` 会
# 以非零退出（`ambiguous argument 'head'`），若 connector 把它当故障，
# 就落进了「问了但没拿到」。**`no_head` 与 `not_a_repo` 与 `unavailable`
# 是三句不同的话**，用户要看到的是「这个仓库还没有提交」。
# ---------------------------------------------------------------------------
echo
echo "== 7 空仓库：201 + no_head =="
git_q -C "$WS_EMPTY" init -q -b main 2>/dev/null
# 自检：这个目录确实是「git 仓库但没有提交」。
if git -C "$WS_EMPTY" rev-parse HEAD >/dev/null 2>&1; then
  bad "$WS_EMPTY 居然有 HEAD——现场没准备好，这一条会误判"
else
  ok "自检：$WS_EMPTY 是仓库且没有 HEAD（rev-parse 退出非零）"
fi
api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS_EMPTY\",\"agent_id\":\"$AID\"}"
if [ "$HTTP_CODE" = "201" ]; then
  ok "空仓库上建会话回 201"
  ST=$(printf '%s' "$LAST_BODY" | jq -r '.base_rev_status // "<缺>"')
  if [ "$ST" = "no_head" ]; then
    ok "base_rev_status = no_head"
  else
    bad "base_rev_status = $ST（期望 no_head）：空仓库被当成了故障"
    dump "断言 7 状态"
  fi
else
  bad "空仓库上建会话回 $HTTP_CODE（期望 201）"
  dump "断言 7"
fi

# ---------------------------------------------------------------------------
# 8 三档在 API 上**互不相同**（同一个列表里并排看）
#
# 单看每一条都绿，仍可能因为某个实现把它们全折成同一个字符串而"通过"——
# 只要那个字符串恰好是每条期望的那个。并排比一次才排得掉。
# ---------------------------------------------------------------------------
echo
echo "== 8 列表里三种状态可区分 =="
api GET /api/sessions
STATUSES=$(printf '%s' "$LAST_BODY" | jq -r '[.[].base_rev_status] | unique | join(",")')
if printf '%s' "$STATUSES" | grep -q 'ok' &&
  printf '%s' "$STATUSES" | grep -q 'not_a_repo' &&
  printf '%s' "$STATUSES" | grep -q 'no_head'; then
  ok "列表里同时出现 ok / not_a_repo / no_head（$STATUSES）"
else
  bad "三档没有同时可区分（实际：$STATUSES）"
  dump "断言 8"
fi
# `unknown` 只由迁移默认值产生，真机上不该出现——出现了说明某条路径漏了探测。
if printf '%s' "$STATUSES" | grep -q 'unknown'; then
  bad "新建的会话出现了 unknown（$STATUSES）：那是迁移默认值，不该由真机产生"
else
  ok "没有 unknown（新建会话都应被探测过）"
fi
# `unavailable` 在健康的真机上也不该出现：它意味着探针没有走通。
if printf '%s' "$STATUSES" | grep -q 'unavailable'; then
  bad "出现了 unavailable（$STATUSES）：探针在某条路径上没走通"
else
  ok "没有 unavailable（探针在三条路径上都走通了）"
fi

# ---------------------------------------------------------------------------
# 9 零残留：进程与临时目录
#
# **必须先杀进程再验目录**：先验目录的话，进程还活着、目录已删会给出一个
# 「干净」的假象。
# ---------------------------------------------------------------------------
echo
echo "== 9 跑完零进程残留 =="
# **先杀 SSE，再杀 Hub**。顺序是有理由的：`afhub` 走 `with_graceful_shutdown`
# （见 `crates/hub-server/src/main.rs:91`），它会等**在飞的连接**收尾完才退，
# 而本脚本自己那条 `curl -N /api/stream` 正是这样一条永不结束的连接。先
# TERM Hub 的话，它会一直等这条 SSE——10 秒都等不完，于是这条断言把「优雅
# 关闭在正常工作」读成了「进程没退干净」。
kill -TERM -"${SSE_PID:-0}" 2>/dev/null || kill -TERM "${SSE_PID:-0}" 2>/dev/null
sleep 0.5
for pid in "${NODE_PID:-}" "${HUB_PID:-}"; do
  [ -n "$pid" ] || continue
  kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null
done
# **退出是异步的**：`kill -TERM` 之后进程还要走完自己的收尾（Hub 要关库、
# 设备要退 worker）。只 sleep 一个固定短时间就断言「还在」会误报——实测里
# 出现过 Hub 在检查之后才退的情况，报出来的是一句假的残留。
# 这里改成轮询：最多等 10 秒，期间全退干净就算过。
# **僵尸不算活着**：`kill -0` 对 `Z` 态进程也返回成功，但僵尸已经不再运行，
# 只是等着父进程 `wait`。本脚本的父进程就是 shell 自己，而那些命令是后台起的，
# 于是它们在检查的窗口里常常正处于 `Z`。判据要读 `ps` 的 STAT 第一列。
alive() {
  local pid=$1 stat
  kill -0 "$pid" 2>/dev/null || return 1
  stat=$(ps -o stat= -p "$pid" 2>/dev/null | tr -d ' ')
  case "${stat%%[A-Za-z]*}" in
    Z*) return 1 ;;
  esac
  [ -n "$stat" ] || return 1
  return 0
}
gone() {
  local pid
  for pid in "${SSE_PID:-}" "${NODE_PID:-}" "${HUB_PID:-}"; do
    [ -n "$pid" ] || continue
    alive "$pid" && return 1
  done
  return 0
}
# **先给 TERM 一个窗口，再升级 KILL，最后才断言。**
#
# 断言要回答的问题是「脚本跑完有没有留下进程」，不是「优雅关闭用了多久」。
# 后者是 Hub 自己的事（`with_graceful_shutdown` 要等 `curl -N /api/stream`
# 那条永不结束的连接收尾，它比脚本里任何一个固定等待都长）。把两件事混在
# 一起，这条断言就会在「Hub 正在优雅关闭」时红——那是一条假红，它会训练人
# 忽略这条断言。
wait_until 10 gone || true
LEFTOVER=0
if ! gone; then
  # TERM 没走完，升级 KILL：脚本的承诺是「跑完不留进程」，不是「只用 TERM」。
  for pid in "${SSE_PID:-}" "${NODE_PID:-}" "${HUB_PID:-}"; do
    [ -n "$pid" ] || continue
    kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null
  done
  wait_until 5 gone || true
fi
if ! gone; then
  for pid in "${SSE_PID:-}" "${NODE_PID:-}" "${HUB_PID:-}"; do
    [ -n "$pid" ] || continue
    if alive "$pid"; then
      LEFTOVER=$((LEFTOVER + 1))
      info "  仍活着的 pid：$pid（stat=$(ps -o stat= -p "$pid" 2>/dev/null)，cmd=$(ps -o args= -p "$pid" 2>/dev/null | cut -c1-80)）"
    fi
  done
fi
if [ "$LEFTOVER" -eq 0 ]; then
  ok "Hub / 设备 / SSE 三个进程都已退出"
else
  bad "$LEFTOVER 个进程没退干净（加 --keep 重跑可保留现场）"
fi
# 端口也放掉了（进程退出 ≠ 端口立刻可复用，但至少不该还被 LISTEN 占着）。
if ss -ltn 2>/dev/null | grep -q ":$PORT "; then
  bad "端口 $PORT 仍在 LISTEN"
else
  ok "端口 $PORT 已释放"
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
