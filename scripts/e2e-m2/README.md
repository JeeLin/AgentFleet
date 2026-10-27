# M2 真设备端到端验收

对应里程碑 `M2-acp-conversation` 的子任务 13。脚本是 `scripts/e2e-m2.sh`。

**这份 README 的存在理由**：M1 的教训是主线一度没读 `scripts/e2e-m1/README.md` §1
（那里明写「不需要真树莓派、自动断言部分不需要浏览器」），就下了「本机跑不了」的错误结论。
M2 多一个 M1 没有的先决条件（设备上得真有一个**能对话**的 agent），所以这里把「什么算够了」
逐条写清楚。

## 1 它验的是什么

起**真** `afhub`、**真** `afcli`（supervisor + worker 两个进程）、**真** `opencode`
（由设备侧的 ACP 连接器亲自 spawn，脚本不直接碰它），然后按页面走的那条路逐条断言：

| 段 | 链路 |
|---|---|
| 页面 → Hub | `POST /api/*`（session cookie + `X-CSRF-Token` 双重提交） |
| Hub → 页面 | `GET /api/stream`（SSE：`channel.frame` / `channel.ready`） |
| Hub ↔ 设备 | `/ws`（协议帧，由真 afcli 自己维持） |
| 设备 → agent | ACP over stdio（`opencode acp`，由 `node-core` 的 `AcpClient` 起） |

这条链路上**没有一处是替身**：写配置走 `POST /api/agents/config`，发消息走
`POST /api/channels/{id}/send`（对话只借了「开通道」那一步，上行仍是页面键盘用的那条裸字节端点），
读回复走 SSE 的 `channel.frame`，读轨迹走 `GET /api/sessions/{id}/trajectory`。

## 2 先决条件

**不需要额外硬件**：本机即合格环境（`scripts/e2e-m1/README.md` §1 已明写，M2 同理）。
不需要浏览器、不需要树莓派、不需要第二台机器。

需要：

| 项 | 要求 | 为什么 |
|---|---|---|
| 工具 | `curl` `jq` `ss` `sqlite3` `awk` `sed` `timeout` | 断言 8 要 `ss`，断言 5 要 `sqlite3` |
| 仓库 | 能 `cargo build --bin afhub --bin afcli` | 二进制不存在时脚本自己构建（`CARGO_BUILD_JOBS=1`） |
| **设备上有 `opencode`** | `command -v opencode` 有输出 | 断言 1/3/4 都靠它 |
| **`opencode` 支持 ACP** | `opencode acp --help` 能跑 | 没有 `acp` 子命令就没有对话 |
| **agent 真能对话** | 见 §3 | 断言 4 的正面路径 |

**磁盘**：这台机器是 40 G 盘。`cargo build` 前确认有 ≥ 1 G 空闲，否则会撞
`No space left on device`（症状是 `cargo` 返回 101 且 shell 完全没有输出）。
`rm -rf target/debug/incremental` 通常能回约 1 G。

**不改仓库里的 `data/`**：Hub 与设备的数据目录都在本次运行的临时目录里
（`AFHUB_DATA_DIR` / `AFCLI_DATA_DIR`）。

## 3 关于「设备上得真有一个能对话的 agent」

这是 M2 比 M1 多出来的先决条件，也是最容易误判的一条。

**本机实测（2026-10-27）**：装有 `opencode v2.1.4`，`opencode acp` 可用，**有可用的模型**，
所以断言 4 能真跑。凭据的位置是：

- 环境变量里（`ANTHROPIC_AUTH_TOKEN` 等）；
- `~/.config/opencode/opencode.jsonc`（`baseURL` 等）。

**但这两处都不是必需的**：实测清空 `HOME`、`env -i`、甚至把端点全部指向黑洞之外的
做法都仍能成功——本机 opencode 有**内置默认模型**。所以：

> 断言 1 的期望值**运行时取自 `opencode --version` 的真实输出**，不写死。
> 写死的话断言退化成「上报值 == 我抄的值」：装了新版仍然是绿的，而「到底有没有被探测出来」
> 一次都没验过。这正是 M1 标准 10（`scripts/e2e-m1.sh:689`）踩过的坑。

## 4 断言清单与「跳过」这一档

| # | 内容 | 档 |
|---|---|---|
| 1 | 心跳上报 agent 运行时，版本与 `opencode --version` 一致；`available=true` | 自动 |
| 1 后半 | 移走 `opencode` → 设备重扫后它消失；还原 → 它回来 | **`--uninstall-drill`** |
| 2 | agent 配置写进去 / 读回来一致 | 自动 |
| 3 | 选已装 agent 建会话 201；选未装 409 且 `error` 可读 | 自动 |
| 4 | 发一条消息 → `stopReason=end_turn` 且拼出非空的 assistant 文本 | 自动 |
| 5 | 轨迹里 assistant ≥1、`turn_end` 到位，五个硬要求可判非空，耗时列按来源判 | 自动 |
| 6 | 重连后沿用同一 `acp_session_id`（走 load 不是 new），轨迹行数不变 | 自动 |
| 7 | 对话之后心跳仍在推进、设备仍 `online` | 自动 |
| 8 | 设备进程零入站监听（`ss -tlnp`） | 自动 |

**「跳过」是独立的一档，不计入通过。** 合并成一个计数就是最该防的那种假绿：
报告里一行 ✅ 覆盖了一段从未执行的断言。脚本末尾会明说有 N 项**没跑**。

> 断言 5 **不断言 `user_message_chunk`**。实测 opencode v2.1.4 的**实时**一轮只推
> `available_commands_update` / `agent_message_chunk` / `usage_update`；
> `user_message_chunk` **只在 `session/load` 的重放里出现**（那是断言 6 走的路）。
> 早先这里要求「轨迹里必须有 user_message_chunk」，于是它永远红——红的原因是
> 那条断言拿**重放数据**当实时数据的期望，不是产品缺东西。
> 用户消息的真相源是 `POST /api/channels/{id}/send` 的请求体，页面手里就有。

### 断言 1 后半为什么要单独开开关

它要**临时把 `opencode` 从磁盘上移走**（`mv` 到同目录的临时名）才能验证「重扫之后它消失」。
这一步：

- 需要写权限才能改 `opencode` 所在的目录；
- 会重启设备进程（探测发生在启动时，改 PATH 不会让已跑的进程重扫）；
- **失败时可能留下被移走的 opencode**。

所以清理函数里**先还原再删临时目录**，且脚本对「移不动」单独报一条 ❌ 而不是静默跳过。

#### 这个 drill 曾经演错了东西（2026-10-27 实测，两处）

第一版只移走 `readlink -f "$(command -v opencode)"` 那**一个**入口，于是它演的不是
「卸载」而是「装坏了」，而这两件事按契约**必须表现不同**：

| 探测结果 | 场景 | 该不该出现在 `/api/agents` |
| --- | --- | --- |
| `NotInstalled` | 从没装过 | **不该** |
| `Failed` | 装了、跑不起来 | **该**，且要带原因 |
| `NotCapable` | 装了、能跑、没有 ACP 入口 | **该**，且要带原因 |

本机 `PATH` 里有**两个** opencode 入口（真二进制 + mise 的 shim）。只移走第一个时，
重扫找到了剩下的 shim——它**能被找到但跑不起来**（目标二进制不在了，退出码非 0），
于是结果是 `Failed`「退出码 1」。按上表那条 red 是**对的**：opencode 确实装坏了，
就该留在目录里让用户看见。

所以 drill 现在**枚举 `PATH` 里所有 `opencode`** 逐个移走，并先自证一句
「移走后 shell 里也找不到 opencode」——没有这一句，drill 演成「装坏了」时
整条链路仍然全绿，**那条绿是假的**。

第二处：还原之后**也要重启设备进程**。第一版只在卸载前重启，还原后不重启，
于是「回来了」永远过——而那条红同样是假的：`agent_runtime` 里确实只剩 `claude`，
opencode 的消失是对的，回来只是**没再扫一眼**。

> 教训：**drill 自己的前提也要断言**。「移走了一个入口」不等于「这台机器上没有它」；
> 「还原了文件」不等于「设备认为它回来了」。

#### 顺带修掉的产品缺陷：设备曾上报未安装的 agent

这个 drill 第一次跑红时（`5 ❌`），查 `agent_runtime` 发现 opencode 那行是
`available=0 / probe_error=∅`，而移走 opencode 前它是好的。根因不在 Hub：

`node-core` 的 `probe_once` 把 `KNOWN_AGENTS` **全部 9 条**都推进心跳，
`Probe::NotInstalled` 转成 `{available:false, error:null, version:null}`。
Hub 侧那一行的形状与「装了但不能对话」（`NotCapable`）**完全相同**，分不开。
于是每台设备都给目录贡献 9 条「没装」的 agent。

修法是让 `report_of` 对 `NotInstalled` 返回 `None`、`probe_once` 跳过它：
心跳的语义是「这台机器上**存在**哪些 agent」，不是「我知道的 9 个 agent 各自怎么样」。
判据从 Hub 侧猜字段收回到了源头，`NotCapable` / `Failed` 两条契约原样保留。

> 别在 Hub 侧用「`probe_error` 是否为空」区分：那只因为 `report_of` 的
> `NotInstalled` 分支恰好没填 `error`，不是契约保证的。谁以后给它补一句可读原因，
> 判据就静默失效，而测试照样绿。

### 断言 4 的判断顺序（不能反）

**环境失败的形态是 `session/new` 就回 JSON-RPC error，不是 `stopReason`。** 实测（黑洞法）：

```json
{"jsonrpc":"2.0","id":2,"error":{"code":-32603,
 "message":"Internal error: OpenCode server is unavailable",
 "data":{"errorName":"ServerUnavailable"}}}
```

所以脚本的顺序是：

1. 先看通道有没有拿到 `ready`（= `session/new` 拿到了 `sessionId`）；
2. 再看 `turn_end` 的 `stopReason` 是不是 `end_turn`。

只盯 `stopReason` 会把环境失败读成「prompt 没回 `end_turn`」，与「对话实现坏了」混为一谈。
两段的超时也分开（`ready` 60 s / `turn_end` 180 s），因为它们的失败含义不同。

### 首 token 延迟波动很大：超时按最坏值

实测同一提示词两次 **2.93 s / 14.53 s**；长回复首个 chunk **8.29 s**。
所以 `ready` 给 60 s、`turn_end` 给 180 s。**不能把「慢」判成「失败」**——
在零 swap 的 3.7 GiB 机器上，一次冷启动的 ACP 子进程本来就要好几秒。

### `session/load` 的重放先于回执到达（断言 6 的判据）

断言 6 判「轨迹行数在 load 前后不变」，前提是**重放不写库**。这条能成立的
时序依据是实测出来的，不是推断：

```
NOTIFY user_message_chunk → NOTIFY agent_message_chunk → RESPONSE(load) → available_commands_update
```

也就是说**重放全部在 load 的回执之前到**，而回执之后只有一条
`available_commands_update`（状态快照，没有 `messageId`）。因此设备侧把
「回执到达」当作解除重放抑制的边界，正好切在重放与后续实时事件之间。

> 边界解除点必须在**读循环处理回执的那一刻**，不在 `load_session()` 返回之后。
> 后者会留下一段窗口：回执之后那条通知可能抢在解除之前被读循环处理掉，
> 而被当成重放丢弃。实测稳定复现（读循环里打印
> `dispatch kind=available_commands_update replaying=true`），连跑 8 次红 4 次。

配套地，去重不能只靠「重放被抑制」：
- 有 `messageId` 的事件按 `(session_id, kind, messageId)` 去重——实测重放那条与
  实时那条的 `messageId` **逐字相同**（`msg_17af87…`）；
- 没有 `messageId` 的**状态快照**按「剔除 `seq` 后 payload 相同」去重，
  白名单目前只有 `available_commands_update`。**`usage_update` 不在里面**：
  它带计数，按内容去重会让计数不变的时期看起来像「没有事件」。

### 跑测试必须串行（本机）

`cargo test` 在本机（3.7 GiB / 零 swap）并行跑会出**假红**：`ETXTBSY`
（刚写的可执行脚本被并发的 fork 继承写句柄）、pty 环境读取竞态、
`/proc` 内存比对抖动。这些在**未改动的 HEAD 上同样出现**，单跑或
`--test-threads=1` 必绿。所以 `cargo test -p node-core` 与
`cargo test -p hub-server` 一律加 `--test-threads=1`。

磁盘满会让 shell 完全无输出（连 `echo` 也没有）。`rustc-LLVM ERROR: IO failure
on output stream: No space left on device` 就是它。清 `target/debug/incremental`
与重复 rlib 能立刻救回来。

## 5 已知未观测

如实写明，不假装是结论：

**拿到会话之后、`session/prompt` 阶段因模型不可用而失败的形态。**
本机剥不掉凭据（opencode 有内置默认模型），黑洞法又在 `session/new` 就断了。
已知的只有「更早一步失败」的形态（见 §4）。如果哪天在无模型的机器上跑，那段输出是要补齐的事实来源。

## 6 怎么跑

```bash
bash scripts/e2e-m2.sh                    # 全自动，结束即清理
bash scripts/e2e-m2.sh --keep             # 保留现场（临时目录路径会打出来）
bash scripts/e2e-m2.sh --uninstall-drill  # 追加断言 1 的后半（会临时移走 opencode）
bash scripts/e2e-m2.sh --port 19000       # 指定 Hub 端口
bash scripts/e2e-m2.sh --help             # 看用法
```

退出码：`0` 全部通过（**且 `skip=0`** 时才可信「全绿」）；`1` 有失败；`2` 先决条件不满足。

## 7 它不验什么

- **浏览器走查**（`design-system.md` 的视觉/键位核对）。脚本只走 HTTP 与 SSE，
  不驱动浏览器；那一半按里程碑的设计核对点人工做。
- **任务分发**。用户已裁定「现在先只做每个设备内自己的对话，不做任务分发」。
- **agent 安装**。已撤销：「让 Hub 往设备写可执行文件 = 一处配置错、全体设备执行未核对代码」。
  agent 由用户自己装，Hub 只下发**启动配置**。
