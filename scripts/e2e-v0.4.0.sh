#!/usr/bin/env bash
# v0.4.0 真设备端到端验收（里程碑《v0.4.0-git-panel》子任务 6）。
#
# ## 它到底做了什么
#
# 起**真** afhub、起**真** afcli（`afcli run`），在一个**真 git 仓库**上按页面
# 走的那条路逐条断言 git 面板的三件事：
#
#   页面 → Hub：`POST /api/channels`（开 git 通道）+ `POST /api/channels/{id}/send`
#   Hub → 页面：`GET /api/stream`（SSE，`channel.frame` / `channel.ready`）
#   Hub ↔ 设备：`/ws`（协议帧，由真 afcli 自己维持）
#   设备 → git：`node-core` 的 git connector 亲自 spawn `git`（脚本不直接调 git 断言页面数据）
#
# 这条链路上**没有一处是替身**，也**不需要 agent**：git connector 与 agent 无关，
# 所以本脚本没有 M2 那条「设备上得有一个能对话的 agent」的先决条件。
#
# ## 断言分级
#
# 全部自动断言，失败即非零退出。没有「跳过」档：这里的先决条件（`git` + 一个临时
# 仓库）本机必然满足，缺了就是环境坏了（退出码 2），不该画成 ⏭️。
#
# ## 用法
#
#   bash scripts/e2e-v0.4.0.sh              # 全自动跑一遍，结束即清理
#   bash scripts/e2e-v0.4.0.sh --keep       # 保留日志现场（临时目录路径会打出来）
#   bash scripts/e2e-v0.4.0.sh --port 19000 # 指定 Hub 端口
#
# 先决条件见 `scripts/e2e-v0.4.0/README.md`。**不改仓库里的 `data/`**：Hub 与设备的
# 数据目录、以及被观察的 git 仓库，都在本次运行的临时目录里。
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
skipped() { echo "  ⏭️  $1"; skip=$((skip + 1)); }
info() { echo "     $1"; }

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
  if [ "$KEEP" -eq 1 ]; then
    echo "  现场保留在：$TMP"
  else
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
echo "== 0 先决条件 =="
missing=""
for tool in curl jq ss sqlite3 awk sed git; do
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

# ---------------------------------------------------------------------------
# 现场：临时目录 + **真 git 仓库** + 端口
#
# 仓库里刻意造三种状态，各验一件事：
#   1. 一个已提交文件被改（worktree 侧 `M`）→ 验 status 的改动列表 + 文件 diff
#   2. 一个未跟踪文件（`??`）        → 验两列状态码原样透出
#   3. 两条提交，各自改一个不同文件   → 验点提交取的是**那条提交**的 diff
# ---------------------------------------------------------------------------
TMP_BASE=${TMPDIR:-/tmp}
mkdir -p "$TMP_BASE" 2>/dev/null || TMP_BASE=/tmp
TMP=$(mktemp -d "$TMP_BASE/agentfleet-e2e-v040.XXXXXX") || {
  echo "  ❌ 无法创建临时目录（TMPDIR=$TMP_BASE）" >&2
  exit 2
}
mkdir -p "$TMP/hub" "$TMP/node" "$TMP/ws"
WS=$TMP/ws

# 用 --initial-branch 而不是改配置：配置是全局的，脚本不该动用户环境。
git -C "$WS" init -q --initial-branch=main 2>/dev/null || git -C "$WS" init -q
# 提交者身份**只在本仓库内**设（`-c` 而不是 `git config`），不写用户的全局配置。
gitc() {
  git -C "$WS" -c user.name='AgentFleet E2E' -c user.email='e2e@example.invalid' \
    -c commit.gpgsign=false "$@"
}
printf 'line one\n' >"$WS/edited.txt"
printf 'first commit content\n' >"$WS/first.txt"
gitc add edited.txt first.txt >/dev/null
gitc commit -q -m 'first: add two files' >/dev/null
# 第二次提交只改 first.txt。**它必须是 HEAD**：断言 6 点的是 HEAD 这条提交，
# 期望它的 diff 里有 'second commit content'、没有工作区未提交的 'line two added'。
# （第一版把 HEAD~1 当目标，而 HEAD~1 是首次提交——它压根不含这段文字，
#  于是断言永远红，红的原因看起来像产品取错了基线。夹具顺序错了。）
printf 'second commit content\n' >"$WS/first.txt"
gitc add first.txt >/dev/null
gitc commit -q -m 'second: touch first.txt' >/dev/null
# 工作区改动：edited.txt 改一行（未提交），再放一个未跟踪文件。
printf 'line one\nline two added\n' >"$WS/edited.txt"
printf 'untracked\n' >"$WS/untracked.txt"

HEAD_HASH=$(git -C "$WS" rev-parse HEAD)
PREV_HASH=$(git -C "$WS" rev-parse HEAD~1)
BRANCH=$(git -C "$WS" rev-parse --abbrev-ref HEAD)
echo "  现场：$TMP（Hub 端口 ${PORT:-自动}，仓库 $WS，branch=$BRANCH HEAD=${HEAD_HASH:0:7}）"
info "HEAD=${HEAD_HASH:0:7} 只改过 first.txt（断言 6 点它，用来抓「取错基线」）"
info "HEAD~1=${PREV_HASH:0:7} 是首次提交（两条提交即可，不需要更多）"

HUB_LOG=$TMP/hub.log
SSE_LOG=$TMP/sse.log
COOKIES=$TMP/cookies.txt
BODY=$TMP/last-body.json
NODE_LOG_DIR=$TMP/node/agentfleet
# Hub 的库文件。断言 1 要读 `device.capabilities`（`/api/devices` 的对外契约里
# **没有** connectors 字段——它只给 `agents` / `usable_agents`）。路径在这里定
# 一处：散在各断言里各拼一次，改目录名时会漏改一处而静默读空。
HUB_DB=$TMP/hub/agentfleet.db

if [ -z "$PORT" ]; then
  for _ in $(seq 1 20); do
    PORT=$(((RANDOM % 2000) + 18000))
    ss -ltn 2>/dev/null | grep -q ":$PORT " || break
  done
fi
BASE="http://127.0.0.1:$PORT"

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

PW='correct horse battery'
SETUP_TOKEN=$(sed 's/\x1b\[[0-9;]*m//g' "$HUB_LOG" | sed -n 's/.*setup token.*: *\([A-Za-z0-9_-]*\).*/\1/p' | head -1)
if [ -z "$SETUP_TOKEN" ]; then
  echo "  ❌ 日志里找不到首次 setup token（Hub 的文案变了？）" >&2
  tail -20 "$HUB_LOG" >&2
  exit 1
fi
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
  echo "  ❌ cookie jar 里没有 af_csrf：后续每个 POST 都会 403" >&2
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
# 1 设备如实上报了 git connector（约束 9 的心跳不撒谎）
# ---------------------------------------------------------------------------
echo
echo "== 1 心跳上报 connectors 含 git =="
# **从 `device.capabilities` 读，不从 `/api/devices` 读**：那个端点的对外契约
# 只有 `{id,name,platform,node_version,status,last_seen,cpu,mem,gpu,agents,usable_agents,derived,key_fingerprint}`
# ——**没有 connectors**。早先这里按 `.[0].capabilities.connectors` 取，拿到的是空串，
# 于是断言 1 永远红，而红的原因看起来像「设备没上报 git」，实际是「脚本读了一个
# 契约里不存在的字段」（M2 的 README §3 记过同一坑）。
connectors_ready() {
  # `jq` 读空串时退出码是 0，所以判据是「取到的值非空」，不是 jq 成功与否。
  local got
  got=$(sqlite3 "$HUB_DB" \
    "select json_extract(capabilities, '$.connectors') from device where id='$NODE_ID';" 2>/dev/null)
  printf '%s' "$got" | grep -q '"git"'
}
if wait_until 30 connectors_ready; then
  GOT_CONN=$(sqlite3 "$HUB_DB" \
    "select json_extract(capabilities, '$.connectors') from device where id='$NODE_ID';" 2>/dev/null)
  ok "设备上报 connectors=$GOT_CONN（含 git）"
else
  bad "30s 内 device.capabilities 的 connectors 里没有 git——通道门禁会 409"
  info "  实际存的值：$(sqlite3 "$HUB_DB" \
    "select json_extract(capabilities, '$.connectors') from device where id='$NODE_ID';" 2>/dev/null)"
  dump "断言 1"
fi

# ---------------------------------------------------------------------------
# 2 建会话（agent 无关：git connector 不需要 agent）
# ---------------------------------------------------------------------------
echo
echo "== 2 建会话 =="
SID=""
# `agent_id` **是必填的**（M2 起：缺了是 400 `agent_required`）。本脚本验的是
# git connector，与 agent 无关，但会话这一层要求它——所以取「设备上已装且可用」
# 的第一个；一个都没有时退回任意已装的名字（建会话会 409，那时才该红）。
AID=$(sqlite3 "$HUB_DB" \
  "select agent_id from agent_runtime where node_id='$NODE_ID' and available=1 limit 1;" 2>/dev/null)
if [ -z "$AID" ]; then
  AID=$(sqlite3 "$HUB_DB" \
    "select agent_id from agent_runtime where node_id='$NODE_ID' limit 1;" 2>/dev/null)
fi
if [ -z "$AID" ]; then
  echo "  ❌ 设备上一个 agent 都没上报：会话建不起来（这不是 git 的问题）" >&2
  dump "断言 2"
  exit 1
fi
info "会话带的 agent_id=$AID（与 git 无关，但会话这一层必填）"
api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$WS\",\"agent_id\":\"$AID\"}"
if [ "$HTTP_CODE" = "201" ]; then
  SID=$(printf '%s' "$LAST_BODY" | jq -r '.id // empty')
  ok "建会话 201，session=$SID（工作区 $WS）"
else
  bad "建会话回 $HTTP_CODE（期望 201）：$LAST_BODY"
  dump "断言 2"
fi

# ---------------------------------------------------------------------------
# 3 开 git 通道 → 发 status → 看到 branch / 改动文件
# ---------------------------------------------------------------------------
echo
echo "== 3 页面开 git 通道 → status 回帧 =="
CH=""
if [ -n "$SID" ]; then
  api POST /api/channels "{\"session_id\":\"$SID\",\"connector\":\"git\",\"params\":{}}"
  if [ "$HTTP_CODE" = "201" ]; then
    CH=$(printf '%s' "$LAST_BODY" | jq -r '.channel_id // empty')
    ok "开 git 通道 201，channel=$CH"
  else
    bad "开 git 通道回 $HTTP_CODE（期望 201）：$LAST_BODY"
    dump "断言 3 开通道"
  fi
else
  bad "断言 2 没拿到会话号，断言 3 无法开始"
fi

status_ready() {
  [ "$(frames_of "$CH" | jq -r 'select(.op == "status") | 1' 2>/dev/null | grep -c .)" -gt 0 ]
}

if [ -n "$CH" ]; then
  SEND_CODE=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    "$BASE/api/channels/$CH/send" --data-binary '{"op":"status"}')
  if [ "$SEND_CODE" = "200" ] || [ "$SEND_CODE" = "202" ] || [ "$SEND_CODE" = "204" ]; then
    info "status 请求已下发（$SEND_CODE）"
  else
    bad "下发 status 回 $SEND_CODE：$(cat "$BODY")"
    dump "断言 3 下发"
  fi

  if wait_until 20 status_ready; then
    ST=$(frames_of "$CH" | jq -c 'select(.op == "status")' | head -1)
    GOT_BRANCH=$(printf '%s' "$ST" | jq -r '.branch // ""')
    GOT_CLEAN=$(printf '%s' "$ST" | jq -r '.clean')
    GOT_FILES=$(printf '%s' "$ST" | jq -c '[.files[].path]')
    GOT_NOTREPO=$(printf '%s' "$ST" | jq -r '.not_a_repo // false')

    if [ "$GOT_NOTREPO" = "true" ]; then
      bad "设备说这不是 git 仓库，但 $WS 确实是（not_a_repo=true）"
      dump "断言 3"
    fi
    # 分支名与仓库真实分支比，不写死 'main'：`git init` 的缺省分支名随
    # `init.defaultBranch` 配置而变，写死会在这台机器以外全红。
    if [ "$GOT_BRANCH" = "$BRANCH" ]; then
      ok "status 的 branch='$GOT_BRANCH'，与仓库真实分支一致"
    else
      bad "status 的 branch='$GOT_BRANCH' ≠ 仓库真实分支 '$BRANCH'"
    fi
    if [ "$GOT_CLEAN" = "false" ]; then
      ok "status 报 clean=false（工作区确实有未提交改动）"
    else
      bad "status 报 clean=$GOT_CLEAN，但工作区有未提交改动（expected.txt 改过 + 一个未跟踪文件）"
    fi

    # 两个文件都必须出现，且**两列状态码原样透出**（`M ` 或 ` M`、`??`）。
    # 只断言「列表非空」的话，把状态码抹平的实现也能通过。
    EDITED_CODE=$(printf '%s' "$ST" | jq -r '.files[] | select(.path == "edited.txt") | "\(.x)\(.y)"')
    UNTRACKED_CODE=$(printf '%s' "$ST" | jq -r '.files[] | select(.path == "untracked.txt") | "\(.x)\(.y)"')
    if [ "$EDITED_CODE" = " M" ] || [ "$EDITED_CODE" = "M " ]; then
      ok "edited.txt 的状态码是 '$EDITED_CODE'（未暂存的修改，原样透出）"
    else
      bad "edited.txt 的状态码是 '$EDITED_CODE'（期望 ' M' 或 'M '）；实收 files=$GOT_FILES"
    fi
    if [ "$UNTRACKED_CODE" = "??" ]; then
      ok "untracked.txt 的状态码是 '??'（未跟踪，原样透出）"
    else
      bad "untracked.txt 的状态码是 '$UNTRACKED_CODE'（期望 '??'）；实收 files=$GOT_FILES"
    fi
  else
    bad "20s 内没有收到 status 回帧"
    dump "断言 3"
  fi
fi

# ---------------------------------------------------------------------------
# 4 点文件 → diff（工作区未提交改动）
# ---------------------------------------------------------------------------
echo
echo "== 4 点文件取 diff（工作区改动）=="
if [ -n "$CH" ]; then
  SEND_CODE=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    "$BASE/api/channels/$CH/send" --data-binary '{"op":"diff","path":"edited.txt"}')
  if [ "$SEND_CODE" = "200" ] || [ "$SEND_CODE" = "202" ] || [ "$SEND_CODE" = "204" ]; then
    info "diff 请求已下发（$SEND_CODE）"
  else
    bad "下发 diff 回 $SEND_CODE：$(cat "$BODY")"
  fi

  file_diff_ready() {
    [ "$(frames_of "$CH" | jq -r 'select(.op == "diff" and .path == "edited.txt") | 1' 2>/dev/null | grep -c .)" -gt 0 ]
  }
  if wait_until 20 file_diff_ready; then
    DF=$(frames_of "$CH" | jq -c 'select(.op == "diff" and .path == "edited.txt")' | head -1)
    UNI=$(printf '%s' "$DF" | jq -r '.unified // ""')
    TRUNC=$(printf '%s' "$DF" | jq -r '.truncated')
    # 断言**这个文件的具体改动**，不是「非空」：只断言非空的话，把整个工作区
    # 的 diff 一起回过来也能通过。
    if printf '%s' "$UNI" | grep -q '^+line two added$'; then
      ok "diff 里含 edited.txt 新增的那一行（'+line two added'）"
    else
      bad "diff 里没有 edited.txt 的新增行。实收 unified 前 5 行："
      printf '%s\n' "$UNI" | head -5 | sed 's/^/       /'
      dump "断言 4"
    fi
    if printf '%s' "$UNI" | grep -q 'second commit content'; then
      bad "工作区 diff 里出现了已提交内容（'second commit content'）——取错基线了"
    else
      ok "工作区 diff 不含已提交内容（确实是「未提交改动」那一档）"
    fi
    if [ "$TRUNC" = "false" ]; then
      ok "这份小 diff 没有被截断（truncated=false）"
    else
      bad "这份小 diff 报 truncated=$TRUNC，但它的体积远小于帧预算"
    fi
  else
    bad "20s 内没收到 edited.txt 的 diff 回帧"
    dump "断言 4"
  fi
fi

# ---------------------------------------------------------------------------
# 5 切历史 → log 看到提交
# ---------------------------------------------------------------------------
echo
echo "== 5 切历史取 log（limit 生效）=="
if [ -n "$CH" ]; then
  SEND_CODE=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    "$BASE/api/channels/$CH/send" --data-binary '{"op":"log","limit":30}')
  if [ "$SEND_CODE" = "200" ] || [ "$SEND_CODE" = "202" ] || [ "$SEND_CODE" = "204" ]; then
    info "log 请求已下发（$SEND_CODE）"
  else
    bad "下发 log 回 $SEND_CODE：$(cat "$BODY")"
  fi

  log_ready() {
    [ "$(frames_of "$CH" | jq -r 'select(.op == "log") | 1' 2>/dev/null | grep -c .)" -gt 0 ]
  }
  if wait_until 20 log_ready; then
    LG=$(frames_of "$CH" | jq -c 'select(.op == "log")' | head -1)
    NCOMMIT=$(printf '%s' "$LG" | jq -r '.commits | length')
    if [ "$NCOMMIT" -ge 2 ] 2>/dev/null; then
      ok "log 回 $NCOMMIT 条提交（仓库里有 2 条，limit=30 未触及上限）"
    else
      bad "log 只回 $NCOMMIT 条提交（期望 ≥ 2）"
      dump "断言 5"
    fi
    # 顺序是**新→旧**：重排会让「最近改了什么」变成要算的问题。
    TOP_HASH=$(printf '%s' "$LG" | jq -r '.commits[0].hash // ""')
    if [ "$TOP_HASH" = "$HEAD_HASH" ]; then
      ok "第一条是最新提交（${HEAD_HASH:0:7}），顺序新→旧"
    else
      bad "第一条是 ${TOP_HASH:0:7}，期望最新提交 ${HEAD_HASH:0:7}"
    fi
    if [ "$(printf '%s' "$LG" | jq -r '.truncated')" = "false" ]; then
      ok "truncated=false（2 条远不足 30 条）"
    else
      bad "truncated=$(printf '%s' "$LG" | jq -r '.truncated')，但只有 2 条提交"
    fi
  else
    bad "20s 内没收到 log 回帧"
    dump "断言 5"
  fi
fi

# ---------------------------------------------------------------------------
# 6 点某条提交 → 该提交的 diff（**不是**工作区 diff）
#
# 这一条是 v0.4.0 里唯一能抓住「取错基线」的断言：`HEAD` 只改过 first.txt，
# 而工作区里 first.txt 是干净的、edited.txt 是脏的。所以：
#   - 该提交的 diff 里必须有 first.txt 的第二版内容（'second commit content'）
#   - 不该有 edited.txt 的未提交新增行（'line two added'）
#   - 不该有 edited.txt 的文件头（那是别的提交 / 工作区的改动）
# ---------------------------------------------------------------------------
echo
echo "== 6 点提交取该提交的 diff（commit 档）=="
if [ -n "$CH" ]; then
  SEND_CODE=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    "$BASE/api/channels/$CH/send" --data-binary "{\"op\":\"diff\",\"commit\":\"$HEAD_HASH\"}")
  if [ "$SEND_CODE" = "200" ] || [ "$SEND_CODE" = "202" ] || [ "$SEND_CODE" = "204" ]; then
    info "commit diff 请求已下发（$SEND_CODE），目标提交 ${HEAD_HASH:0:7}"
  else
    bad "下发 commit diff 回 $SEND_CODE：$(cat "$BODY")"
  fi

  # 用「第 2 帧之后的 commit diff」区分：判断条件是**新增行**存在，
  # 而不是数帧数（工作区那份 diff 也在同一通道上）。
  commit_diff_ready() {
    frames_of "$CH" | jq -r 'select(.op == "diff") | .unified // ""' 2>/dev/null |
      grep -q '^+second commit content$'
  }
  if wait_until 20 commit_diff_ready; then
    CD=$(frames_of "$CH" | jq -c 'select(.op == "diff")' | grep -F 'second commit content' | head -1)
    CDU=$(printf '%s' "$CD" | jq -r '.unified // ""')
    if printf '%s' "$CDU" | grep -q -- '--- a/first.txt'; then
      ok "该提交的 diff 里有 '--- a/first.txt'（它确实只改了这个文件）"
    else
      bad "该提交的 diff 里没有 first.txt 的文件头。实收前 5 行："
      printf '%s\n' "$CDU" | head -5 | sed 's/^/       /'
      dump "断言 6"
    fi
    # **最关键的一条**：不能混进工作区的未提交改动。
    if printf '%s' "$CDU" | grep -q 'line two added'; then
      bad "该提交的 diff 里出现了工作区未提交的改动（'line two added'）——发的是 git diff 而非 git show"
      dump "断言 6"
    else
      ok "该提交的 diff 不含工作区未提交改动（用的是 git show，不是 git diff <hash>）"
    fi
    # 也不能混进另一条提交（HEAD）改动过的文件。
    if printf '%s' "$CDU" | grep -q 'edited.txt'; then
      bad "该提交的 diff 里出现了 edited.txt——那是别的提交 / 工作区的改动"
    else
      ok "该提交的 diff 只含它自己改过的文件"
    fi
  else
    bad "20s 内没收到该提交的 diff（'second commit content' 从未出现在任何 diff 回帧里）"
    dump "断言 6"
  fi
fi

# ---------------------------------------------------------------------------
# 7 不是 git 仓库 → 正常回帧 not_a_repo:true（**不是** channel_error）
#
# 这条只有真设备能验：`channel.ts` 只往页面传错误码、丢弃 reason，所以「不是仓库」
# 若走错误路径，页面就分不清它和「git 挂了」。
# ---------------------------------------------------------------------------
echo
echo "== 7 非仓库目录 → status 回 not_a_repo:true =="
PLAIN=$TMP/plain
mkdir -p "$PLAIN"
printf 'just a file\n' >"$PLAIN/x.txt"
SID2=""
api POST /api/sessions "{\"node_id\":\"$NODE_ID\",\"path\":\"$PLAIN\",\"agent_id\":\"$AID\"}"
if [ "$HTTP_CODE" = "201" ]; then
  SID2=$(printf '%s' "$LAST_BODY" | jq -r '.id // empty')
  info "第二个会话 $SID2 落在非仓库目录 $PLAIN"
else
  bad "在非仓库目录建会话回 $HTTP_CODE（期望 201）：$LAST_BODY"
fi

CH2=""
if [ -n "$SID2" ]; then
  api POST /api/channels "{\"session_id\":\"$SID2\",\"connector\":\"git\",\"params\":{}}"
  if [ "$HTTP_CODE" = "201" ]; then
    CH2=$(printf '%s' "$LAST_BODY" | jq -r '.channel_id // empty')
    ok "非仓库目录上也能开 git 通道 201，channel=$CH2"
  else
    bad "非仓库目录开 git 通道回 $HTTP_CODE（期望 201）：$LAST_BODY"
  fi
fi

if [ -n "$CH2" ]; then
  SEND_CODE=$(curl_auth -o "$BODY" -w '%{http_code}' -X POST \
    "$BASE/api/channels/$CH2/send" --data-binary '{"op":"status"}')
  if [ "$SEND_CODE" = "200" ] || [ "$SEND_CODE" = "202" ] || [ "$SEND_CODE" = "204" ]; then
    info "status 请求已下发（$SEND_CODE）"
  else
    bad "下发 status 回 $SEND_CODE：$(cat "$BODY")"
  fi

  notrepo_ready() {
    [ "$(frames_of "$CH2" | jq -r 'select(.op == "status") | 1' 2>/dev/null | grep -c .)" -gt 0 ]
  }
  if wait_until 20 notrepo_ready; then
    NR=$(frames_of "$CH2" | jq -c 'select(.op == "status")' | head -1)
    if [ "$(printf '%s' "$NR" | jq -r '.not_a_repo // false')" = "true" ]; then
      ok "非仓库回正常 status 帧且 not_a_repo=true（不是 channel_error）"
    else
      bad "非仓库的 status 帧里 not_a_repo 不是 true：$NR"
      dump "断言 7"
    fi
    # `clean` 必须是 null：说 true 会让页面在非仓库上显示「没有未提交的改动」。
    if [ "$(printf '%s' "$NR" | jq -r '.clean')" = "null" ]; then
      ok "非仓库的 clean 是 null（不是 true——那会在页面上写「没有未提交的改动」）"
    else
      bad "非仓库的 clean 是 $(printf '%s' "$NR" | jq -c '.clean')，期望 null"
    fi
    if [ "$(printf '%s' "$NR" | jq -r '.files | length')" = "0" ]; then
      ok "非仓库的 files 是空数组"
    else
      bad "非仓库的 files 不是空数组：$(printf '%s' "$NR" | jq -c '.files')"
    fi
    # 反证：这条通道上**不该**出现 channel_error。出现就说明还是走了错误路径。
    if grep -q "channel_error" "$SSE_LOG" 2>/dev/null &&
      [ "$(frames_of "$CH2" | wc -l)" = "0" ]; then
      bad "非仓库上只收到了 channel_error、没有 status 帧——still 走错误路径"
      dump "断言 7"
    else
      ok "非仓库这条通道没有落到 channel_error 上"
    fi
  else
    bad "20s 内没有 status 回帧（很可能设备把它当错误回成了 channel_error）"
    info "SSE 里该通道的全部载荷："
    frames_of "$CH2" | head -5 | sed 's/^/       /'
    dump "断言 7"
  fi
fi

# ---------------------------------------------------------------------------
echo
echo "== 结果 =="
echo "  通过 $pass，失败 $fail，跳过 $skip"
if [ "$skip" -ne 0 ]; then
  echo "  ⚠️  有 $skip 项**没跑**：见上面 ⏭️ 行。跳过不计入通过。"
fi
if [ "$fail" -ne 0 ]; then
  echo "  ❌ 验收未通过" >&2
  [ "$KEEP" -eq 0 ] && echo "     （加 --keep 重跑可保留现场）" >&2
  exit 1
fi
echo "  ✅ 验收通过"
