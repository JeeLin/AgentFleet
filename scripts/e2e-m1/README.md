# M1 真设备端到端验收

这是里程碑《M1-web-console》子任务 14 的验收夹具：`scripts/e2e-m1.sh` 起**真**
`afhub` + 真 `afcli`，然后用**页面走的那条路**（`/api/*` + SSE `/api/stream`）
逐条核对 M1 的验收标准。

- 自动断言：标准 **1、2、3、4、5、6、7、10** —— 有一条失败就非零退出。
- 半自动：标准 **8、9** —— 脚本只给**观测值**，「内容不丢 / 不冻结」由人对照
  `docs/design-system.md` §7.3 判断。**脚本不把它们伪装成 ✅**。

> 为什么不能只跑单测：M1 的价值全在「真设备上成立与否」。编译通过、单测全绿，
> 而页面上敲一行命令没有任何反应，这个里程碑就等于没做完。

---

## 1. 先决条件

### 需要

| 项 | 说明 |
|---|---|
| 真 `afhub` / 真 `afcli` | 由本仓库构建（`cargo build --bin afhub --bin afcli`，默认 debug）。脚本发现二进制不存在会自己构建（`CARGO_BUILD_JOBS` 默认 2） |
| 真浏览器 | 标准 8、9 的人工走查；自动断言部分不需要浏览器 |
| Linux | 用到 `ss`（标准 7）、`sqlite3`（标准 10）、`/proc`（进程树核对）、PTY（终端标准）。macOS 上 `ss` 与 `/proc` 都不成立 |
| `curl` `jq` `ss` `sqlite3` | 缺任一项，脚本在「0 先决条件」就退出（**不记为通过**） |
| 一个空闲的回环端口 | 默认在 18000–19999 里随机挑；也可 `--port N` |

### **不需要**

- **不需要真树莓派。** amd64 的运行 Linux 的机器（含开发机 / 容器）就是一台真设备。
  跨设备分发已按 `docs/DECISIONS.md` **D9** 推迟到 M8；M1 的会话**一律绑定单节点**，
  脚本里从头到尾只 enroll 一台设备。
- 不需要 Hub 有到设备的入站访问（约束 1）：设备是主动外联到 `AFCLI_HUB_URL` 的，
  标准 7 专门核对这一点。
- 不需要改仓库里的 `data/`：Hub 用 `AFHUB_DATA_DIR=<临时目录>/hub`，
  设备用 `AFCLI_DATA_DIR=<临时目录>/node`，跑完（不带 `--keep`）整个临时目录删掉。

### 角色说明（本夹具里三者都在同一台机器上）

```
浏览器/脚本 ──HTTP+SSE──▶ afhub (127.0.0.1:<port>) ◀──WS── afcli run (supervisor) ──▶ afcli run --worker
```

同一台机器不改变任何判定：设备侧仍然**只主动外联**，Hub 仍然**没有任何到设备的入站端口**。
换到另一台机器只是把 `AFCLI_HUB_URL` 指过去（那时 Hub 的回环地址要换成 Tailscale / SSH 端口转发）。

---

## 2. 怎么跑

```bash
# 1) 构建（脚本也会自动做）
CARGO_BUILD_JOBS=2 cargo build --bin afhub --bin afcli

# 2) 跑一遍
bash scripts/e2e-m1.sh

# 常用开关
bash scripts/e2e-m1.sh --keep            # 保留现场：打印临时目录，日志与 SSE 全在里面
bash scripts/e2e-m1.sh --offline-drill   # 追加标准 9 的状态机观测（约 1 分钟）
bash scripts/e2e-m1.sh --hold            # 跑完挂住，让你在浏览器里走查 8/9，回车才清理
bash scripts/e2e-m1.sh --port 19000      # 固定 Hub 端口
TMPDIR=/tmp/opencode bash scripts/e2e-m1.sh   # 换临时目录根（默认 /tmp）
```

**可重复执行（幂等）**：每次运行都是新的临时目录 + 新的随机端口；`trap` 在
正常结束、断言失败、Ctrl-C 三条路径上都收掉 `afhub` 与整组 `afcli`
（含 `run --worker` 子进程），收不干净会明确报 ❌ 并以非零退出。

退出码：`0` = 自动断言全过；`1` = 有断言失败 / 有进程残留；`2` = 先决条件不满足（缺工具、建不出临时目录、Hub 起不来）。

---

## 3. 脚本到底走了哪条路

每一步都是**页面真的会发的那个请求**，不是为测试造的后门：

| 步骤 | 调用 | 页面里的对应物 |
|---|---|---|
| 设密码 | `POST /setup` + `x-setup-token`（读 `<data>/setup-token.txt`） | `SetupPage.vue` |
| 登录 | `POST /login` → `af_session` + `af_csrf` cookie | `LoginPage.vue` |
| 订阅 | `GET /api/stream`（SSE） | `shared/channel.ts` 的 `EventSource` |
| 接入设备 | `AFCLI_DATA_DIR=… AFCLI_HUB_URL=ws://…/ws afcli run` **一条命令** | 用户敲的那条命令 |
| 建会话 | `POST /api/sessions {node_id, path}` | `SessionPage.vue` 新建会话 |
| 开面板 | `POST /api/channels {session_id, connector, params}` | `shared/channel.ts::open` |
| 敲键盘 | `POST /api/channels/{id}/send`（裸字节） | `TerminalPanel` → `channel.send` |
| 改尺寸 | `POST /api/channels/{id}/resize {cols, rows}` | `TerminalPanel` 的 fit |
| 收输出 | SSE `channel.frame` 的 `payload.bytes` | 同上 |

面板参数与前端**逐字一致**（这是断言的一部分）：

- terminal：`{"cwd": <会话路径>, "cols": 80, "rows": 24}` — `packages/web/src/domains/session/SessionPage.vue:648`
- files：`{}` — 同一个 `hub.handle(key, { connector: panel, params: … })` 调用点，`panel !== 'terminal'` 时走 `{}`（`packages/web/src/domains/session/SessionPage.vue:648`）；工作区根由 Hub 按会话补
- exec：`{"command": …, "args": […], "cwd": ""}` — `runCommand()` 里的 `hub.handle(key, { connector: 'exec', params: … })`（`packages/web/src/domains/session/SessionPage.vue:2172`）

---

## 4. 每条标准怎么判

| 标准 | 判据（脚本里的可观测事实） | 级别 |
|---|---|---|
| 1 `afcli run` 一条命令接入 | `/api/devices` 出现该设备且 `status=online`；supervisor + `run --worker` 两层进程都在；API 只回 `key_fingerprint`、无密钥字段 | 自动 |
| 2 建会话 | `POST /api/sessions` → 201，`workspace={node_id,path}`、`state=running`；`GET /api/sessions` 里能看到 | 自动 |
| 3 终端：从页面敲 echo / resize | ①`stty size` → `24 80`（初值从**前端源码**读出——常量 `DEFAULT_COLS/DEFAULT_ROWS`（`packages/web/src/shared/channel.ts`）或页面里的 `cols:/rows:`，读不到就拒绝运行）②`POST /resize 132x40` 后 `stty size` → `40 132` ③`/send` 敲 `echo <marker>` 后，该通道字节流里 marker 出现 **≥2 次** | 自动 |
| 4 文件：浏览目录树、读文件 | 开 files 通道后 `send {"op":"list","path":"."}`，SSE 上出现 `op=list` 且 `entries` 含 `a.txt`/`src`；`{"op":"read","path":"a.txt"}` 回 `text` 含文件内容 | 自动 |
| 5 越界被拒且错误可读 | 三种构造各发一次 `{"op":"read"}`：`../outside/secret.txt`、`/etc/hostname`、`link-outside.txt`（真符号链接指向界外）——每种都要求页面上出现一条**带原因**的拒绝信号（`channel.frame` 的 `error/code/reason`，或 `channel.error` 事件） | 自动 |
| 6 exec：一次性命令 | `{"command":"sh","args":["-c","echo OUT; echo ERR >&2; exit 7"],"cwd":""}` → 回帧 `exit_code=7`、`stdout_b64` 解码为 `OUT`、`stderr_b64` 解码为 `ERR` | 自动 |
| 7 零入站端口 | `ss -tlnp`：`afcli`（supervisor + worker 两个 pid）没有任何 `LISTEN`；只有一条到 Hub 的 `ESTAB`；Hub 自己只监听 `127.0.0.1:<port>` | 自动 |
| 10 心跳能力表 == 注册表 | `sqlite3 <hub>/<库文件名> "select capabilities from device"` 的 `connectors` == `exec,files,terminal`；库文件名从 `crates/hub-storage/src/db.rs` 的生产取值读出，夹具不写死；`agents` 为空；三个**已上报**的 connector 都能被 Hub 下发，未上报的 `git` 被 409 `connector_not_reported` 挡在 Hub | 自动 |
| 8 多会话 tab | 开 3 个会话各一条终端通道，敲各自 marker；打印每通道帧数、marker 次数、`channel.ready`/`channel.closed` 增量 | **半自动** |
| 9 拔网线 → 离线 → 恢复 | 打印 `device.status` 时间线 + `/api/devices.status` 采样；`--offline-drill` 可用 SIGSTOP/SIGCONT 代跑「设备不再回应」 | **半自动** |

### 标准 3 的「≥2 次」是什么意思

marker 第一次出现是 PTY 把**敲进去的字符回显**，第二次才是 `echo` 的**输出**。
只断言「有输出」的话，设备侧预置一条命令就能过——而标准 3 要验的恰恰是
「页面 → Hub → 设备 `/send`」这一段真的通。只出现 1 次 = 字进了 PTY、命令没跑
（曾经的真实故障形态：`$(printf '\n')` 被命令替换吃掉，发过去是一句没有回车的台词）。

### 标准 5 的「错误可读」按**页面可见**算

设备侧能正确拒绝、但页面上一片空白，对用户等于「没有错误」。
所以判据是「页面上能看到一条带原因的信号」，而不是「设备日志里有」。
`docs/design-system.md` §7.4 也是这个口径：越界要有**四路可见反馈**，
其中第 ③ 路就是「解释性错误面板：为什么进不去 + 怎么才能进去」。

---

## 5. 观测值怎么读（标准 8 / 9）

### 标准 8：多会话 tab

脚本会打印，例如：

```
会话 <id>：通道 <id>，敲入 AF_TAB2_26161 → 该通道 6 条帧，AF_TAB2_26161 出现 2 次
本轮的通道账（只看这一段）：channel.ready +2 条、channel.closed +0 条
会话数：3
```

- `channel.ready +N / channel.closed +0`：这一段里只建不关，说明切 tab **没有**把通道关掉
  （`SessionPage.vue` 的边界是「关面板才 close，切 tab 不关」）。
- 每个通道的 marker 出现次数：≥2 说明那条 PTY 的输出还在往下走。
- **脚本给不了的两件事**：屏幕上的内容有没有留在那个 tab 里、面板的滚动位置对不对。
  这两件只能在浏览器里看。

人工走查（3 分钟）：

1. `bash scripts/e2e-m1.sh --hold`：跑完自动断言后**不清理**，打印 `http://127.0.0.1:<port>/`
   与密码（脚本里的 `PW` 变量，默认 `correct horse battery`），等你在浏览器里走查，
   回车后才收进程（`--hold` 结束时现场目录也会保留）。
2. 打开上面那个地址登录，用同一台设备、同一个路径建 3 个会话（tab）。
3. 每个 tab 里：开终端敲点东西 → 开文件面板展开两层 → 切到别的 tab 再切回来。
4. 要看到：**通道数不减**（面板头一直是「已连接」，不回退到「建立中」/「建立失败」）、
   **切回来内容还在**（终端历史、文件树展开层、预览都不重来）。

### 标准 9：拔网线

脚本会打印 `device.status` 时间线和状态采样。`--offline-drill` 的读法：

```
演练：SIGSTOP 设备 worker … = 设备不再回应
  t+0s status=online … t+33s status=offline
SIGCONT → 继续观察回到 online
  t+0s status=online
```

- **它复现的是「心跳停了」这一半**：设备不再回应 → 45s 判据走完 → `offline`。
  WS 并没有断（SIGSTOP 不关 socket），所以**不会**出现 `reconnecting`。
- 「拔网线」那一半（WS 断、45s 内 `reconnecting`、终端不冻结）**必须人工做**：
  停掉设备网络（`ip link set <if> down`，或直接停掉 `afcli` 之外的那台机器的网络），
  观察 45s 内与 45s 后两段。
- 期望（`design-system.md` §7.3 的三态表）：
  - `reconnecting`（WS 刚断，< 45s）：琥珀 / `◐`，**终端输入不禁用**，
    文案「设备上的 shell 仍然活着，所以这里不禁用输入」；状态条给丢帧数。
  - `offline`（心跳超 45s）：红 / `○`，**冻结** + 斜纹只读遮罩，
    文案「设备上的 PTY 仍然活着，Hub 只是暂时够不着」；**最后已知内容保留且可全选复制**。
  - 恢复：**自动重连**，不需要刷新页面。
- 「不冻结」的判断不是「界面没白屏」，而是：45s 内输入框**能打字**、命令能发出去、
  状态条在数丢帧，而不是把一台调度器仍认为可用的设备画成死设备。

---

## 6. 本夹具暴露过的产品缺陷（**已全部修掉**，留档供追溯）

本夹具落地时，自动断言在 **4、5、6** 上是红的，原因全在**产品侧**。
四条已由 `0972220`（fix(hub): close the four gaps the real-device E2E run exposed）
修掉，复跑全绿。留在这里是因为**「当时错在哪、后来怎么修的」比「现在是对的」更有信息量**：
下面每条都写了「当时的现象」与「现在的代码位置」，可以直接对着读。

### 6.1 files / exec 面板拿不到工作区根（已修）

- 里程碑 `M1-web-console.md` §9 写的是「会话工作区根目录在 `open_channel` 时由
  **Hub** 塞进 params（Hub 从 `session.workspace_id` 查出 `(node_id, path)`）」。
- 实际：`crates/hub-server/src/routes/stream.rs:574` 的 `open_channel` 如今先调
  `params_with_workspace_root` 把会话工作区根**覆盖**进 params（`:874` 是那个函数），
  而前端 files 面板发的就是 `{}`（`packages/web/src/domains/session/SessionPage.vue:648`）。
- 设备侧：`files` / `exec` 的 `open` 都要求 `OpenReq.workspace_root`
  （`crates/node-connector/src/connector/files.rs:193`、`crates/node-connector/src/connector/exec.rs:186`），
  缺了就直接回 `channel_error`。
- 后果：文件面板永远是一棵空树；`exec` 面板提交命令后没有任何反应。
- **现在**：`open_channel` 调 `params_with_workspace_root`（`crates/hub-server/src/routes/stream.rs:607`）
  把会话工作区根注入 params，浏览器给什么都会被覆盖。

### 6.2 files 的取数载荷在 Hub 就被改写成了终端载荷（已修）

- 页面取数走 `channel.send(JSON.stringify({op:"list",…}))`
  （`packages/web/src/domains/session/SessionPage.vue:993`，`terminalInput()` 里的 `handle.send(text)`）→
  `POST /api/channels/{id}/send`。
- 该端点把**任何**请求体包成 `{"bytes": <text>}`（`crates/hub-server/src/routes/stream.rs:851`）——
  那是 terminal connector 的载荷形状；而 files connector 只认**顶层** `op`
  （`crates/node-connector/src/connector/files.rs:219`）。
- 所以 `{"op":"list"}` 到设备时变成 `{"bytes":"{\"op\":\"list\"…}"}`，
  在边界检查之前就已经走岔了。当年设计上假定「二进制通道（files 读写）走各自的
  connector 载荷，不复用这条裸字节入口」，可**实际没有第二条入口**
  （`/api/channels` 只有 `send` / `resize` / `DELETE`）——那条分离是纸面上的。
- **现在**：`/send` 按 Content-Type 分派（`crates/hub-server/src/routes/stream.rs:959` 的 `payload_kind`）——
  `application/json` 原样、`application/octet-stream` 包成 `{"bytes":…}`、
  其余按内容判断。files 面板走 JSON，与 terminal 的裸字节入口分开。

### 6.3 设备回的 `channel_error` 到不了页面（已修）

- `crates/hub-server/src/routes/ws.rs:855` 只把 `channel_data` 的载荷推给 SSE；`channel_error` 走
  `channels::on_channel_frame` 后落进 `_ => Reply::Nothing`，**不转发**。
- 后果：即使 6.1 / 6.2 修好，标准 5 的「越界被拒且**错误可读**」在页面上仍然是
  一片空白——面板停在「已连接」，用户看不到「为什么进不去」。
  这与 `design-system.md` §7.4 的第 ③ 路（解释性错误面板）直接冲突。
- **现在**：`crates/hub-server/src/routes/ws.rs:861` 识别 `CHANNEL_ERROR` 并调 `publish_channel_error`（`:990`），
  错误因此能到 SSE、到页面。

### 6.4 面板在设备确认之前就显示「已连接」（已修）

- `POST /api/channels` 在把 `open_channel` 塞进设备出站队列之后**立刻**推
  `channel.ready`（当时在 `crates/hub-server/src/routes/stream.rs` 的 `open_channel`
  里——现已改为设备回 `channel_ready` 帧时才发，见该文件的 `channel_ready` 注释），
  而页面把这条事件当成「通道可用了」
  （`shared/channel.ts` 的 `set('ready')`）。设备那边的 `channel_ready` 帧到 Hub 后
  并不会被转发给页面（同 6.3 的口径）。
- 后果：从面板出现到设备 `open` 完成之间的输入，会被设备当成未知通道**静默丢弃**
  （`crates/node-core/src/channel.rs:136`「通道不存在 / 已关：静默」），页面上既没有
  「已丢 N 条」也没有失败标记——打字没反应，用户只会以为卡了。
- 本脚本的应对：像人一样先等**首帧**（提示符）到达再开始敲（`terminal_has_frames`），
  这不是绕过断言——标准 3 要验的是「从页面敲能出结果」，不是「设备打开前敲的字也不许丢」。
  但这条窗口本身是产品该修的（要么等设备 `channel_ready` 再翻 `ready`，
  要么把节点侧的 `channel_error`/丢帧反馈给页面）。
- **现在**：Hub 只在设备回 `channel_ready` 之后才推 `channel.ready` 给页面
  （`crates/hub-server/src/routes/stream.rs:888` 的 `announce_ready`），页面的 `ready` 因此是「设备已确认」而不是「已下发」。

### 怎么区分「脚本 bug」和「产品 bug」

1. 先在浏览器里手工做一遍同样的动作（files 面板点开目录 / exec 提交一条命令）。
   **浏览器里也一样没反应** → 产品缺陷（脚本只是照实报）。
2. 看断言输出的「现场」：通道有 `channel.ready`、`/send` 回 204，但该通道
   **0 条 `channel.frame`** —— 这说明请求被接受、设备没回任何东西（而不是脚本没发）。
3. 用 `--keep` 留下现场，`sqlite3`/日志都在临时目录里；
   `curl` 复跑一次 `POST /api/channels` 看响应体（脚本会把状态码与响应体一起打出来）。

**脚本不会为了变绿去绕过这些缺口**：它不会替 Hub 补 `workspace_root`、不会绕开
`/send` 直接给设备发自定义帧、也不会把「设备日志里有错误」当成「错误可读」。
发现缺陷就停下报告，改在产品侧。

---

## 7. 输出示例（一次真实运行的关键行）

```
== 标准 1 afcli run 一条命令接入，设备出现在 /api/devices ==
  ✅ 设备已在 /api/devices：id=… status=online
  ✅ supervisor/worker 两层都在（约束 12）：supervisor=… worker=…
  ✅ API 只给指纹（约束 13）：cb2***315
== 标准 2 建会话，会话出现在 /api/sessions ==
  ✅ POST /api/sessions → 201，session_id=…
  ✅ 会话绑定了 (node_id, path) 且状态 running：{"state":"running","workspace":{…}}
== 标准 3 终端面板：从页面敲 echo 有输出，stty size 验证 resize ==
  ✅ 页面给的 80x24 到了 PTY：`stty size` → 24 80
  ✅ resize 生效：POST /resize 132x40 后 `stty size` → 40 132
  ✅ 从页面敲的 echo 有输出：AF_E2E_… 出现 2 次（≥2 = 回显 + 命令真的跑了）
== 标准 4 文件面板：浏览工作区目录树、读文件 ==
  ✅ 文件通道已建立（channel_id=…）
  ✅ 列目录成功：README.md,a.txt,link-outside.txt,src
  ✅ 读文件成功：a.txt → hello from the e2e workspace
== 标准 5 越界路径被拒且错误可读 ==
  ✅ 拒绝「../outside/secret.txt」且页面能看到：path outside the workspace: ../outside/secret.txt
  ✅ 拒绝「/etc/hostname」且页面能看到：path outside the workspace: /etc/hostname
  ✅ 拒绝「link-outside.txt」且页面能看到：path outside the workspace: link-outside.txt
== 标准 6 exec 面板：一次性命令，退出码与输出正确 ==
  ✅ exec 结果：{"duration_ms":1,"exit_code":7,"stderr_b64":"RVJSCg==","stdout_b64":"T1VUCg==",…}
  ✅ 退出码 7、stdout=OUT、stderr=ERR 全对
== 标准 7 零入站端口：设备侧不开放任何监听 ==
  ✅ afcli（supervisor=… worker=…）没有任何 LISTEN 套接字
  ✅ 设备只有主动外联（到 Hub 的 … 端口）
== 标准 10 心跳能力表与 node 实际实现的 connector 完全一致 ==
  ✅ 上报的 connectors == 注册表：exec,files,terminal
  ✅ 没上报的 connector 「git」在 Hub 就被拒（409 connector_not_reported）
...
  自动断言：✅ 29 项通过，❌ 0 项失败
```

**这是当前基线**：第 6 节那四条产品缺口修掉后，同一条命令全绿。
若看到 ❌，回去看第 6 节——那里记着每条断言当初为什么红、修在哪。

---

## 8. 清理与现场

- 默认：跑完删掉临时目录（Hub 库、日志、SSE 全文都在里面）。
- `--keep` / `--hold`：保留并打印路径（`--hold` 在浏览器走查结束、按回车之后才清理）。里面值得看的：
  - `hub.log` — Hub 侧日志（ANSI 已可 `sed 's/\x1b\[[0-9;]*m//g'` 去掉）
  - `sse.log` — 页面订阅到的**全部** SSE 事件（`id/event/data` 原样）
  - `node/agentfleet/logs/afcli.log` — 设备日志
  - `node/agentfleet/identity.json` — 本次设备的身份（临时密钥，跑完就没用了）
  - `hub/<库文件名>` — Hub 库（标准 10 就是读它的 `device.capabilities`；文件名取自
    `crates/hub-storage/src/db.rs` 的生产取值，不是夹具写死的）
- `trap` 收进程的顺序：SSE 订阅 → `afcli` 进程组（`TERM`，等 2s，再 `KILL`）→ `afhub`（`TERM`，走优雅关闭）。
