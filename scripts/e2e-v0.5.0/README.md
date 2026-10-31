# v0.5.0 真设备端到端验收（右侧一格 / 换工具即释放）

对应里程碑 `.mdflow/milestones/v0.5.0-opencode-layout.md` 的子任务 6。脚本是
`scripts/e2e-v0.5.0.sh`。

**这份 README 的存在理由**：M1 的教训是主线一度没读 `scripts/e2e-m1/README.md` §1
就下了「本机跑不了」的错误结论；M2 的教训是**脚本读了一个契约里不存在的字段**，
于是断言永远红、红的原因看起来像产品坏了。v0.5.0 又添了一个新形态的坑——
**断言看起来在验某件事，实际恒真**（见 §4）。这三类都写在这里，而不是留在提交消息里。

## 1 它验的是什么，以及为什么不验像素

v0.5.0 是**纯前端**的布局重排：全局侧栏在会话页收起、tab 条置顶、右侧一格、
「＋」换工具、左右一条分栏、窄屏转覆盖式抽屉。

其中「侧栏宽度、tab 条位置、抽屉是否覆盖」这类东西**不在这里验**——那是
`packages/web/src/**/*.test.ts` 的活，而且 DOM 测试比截图稳定得多，跑一次几秒。

本脚本验的是布局重排里**唯一有真实代价**的那一条语义：

> **换工具 = 替换当前工具 + 释放被换掉那个工具的通道**（§7.1.3）

这条的唯一证据在**设备侧**：`close_channel` 到了设备之后，
`crates/node-core/src/connector/terminal.rs:305` 的 `pty.close()` 会**真的杀掉用户的 shell**。
DOM 测试只能证明页面发了 `DELETE /api/channels/{id}`（那是 mock 出来的 `deletes()` 断言），
证明不了设备上那条 PTY 真的没了。所以这里起真 afhub、真 afcli、真 PTY。

| 段 | 链路 |
|---|---|
| 页面 → Hub | `POST /api/channels`（选中工具 = 开通道）、`DELETE /api/channels/{id}`（换掉 = 释放） |
| Hub → 页面 | `GET /api/stream`（SSE：`channel.ready` / `channel.frame` / `channel.error`） |
| Hub ↔ 设备 | `/ws`（`open_channel` / `close_channel`，由真 afcli 自己维持） |
| 设备 → PTY | `node-core` 的 terminal connector 亲自 spawn shell |

这条链路上**没有一处是替身**。

**不需要 git 仓库**：工作区只要存在就行，`cwd` 传给 terminal connector 用来证明
「`open` 真的到了设备、真的 spawn 了 shell」。git 面板的语义归 `scripts/e2e-v0.4.0.sh` 管，
这里再验一遍只是重复。

**不需要 agent 参与断言**，但 `POST /api/sessions` **要求 `agent_id` 必填**（M2 起，
缺了是 400 `agent_required`），所以脚本从 `agent_runtime` 里取一个已装的名字填进去——
它不参与任何断言，只为了让会话建起来。取不到时退出码 2（那不是 terminal 的问题，
是环境不完整）。

## 2 先决条件

**不需要额外硬件、不需要浏览器、不需要树莓派**（`scripts/e2e-m1/README.md` §1 已明写）。

| 项 | 要求 | 为什么 |
|---|---|---|
| 工具 | `curl` `jq` `ss` `sqlite3` `awk` `sed` `sh` | 断言 1 要 `sqlite3`，端口探测要 `ss` |
| 二进制 | 能 `cargo build --bin afhub --bin afcli` | 不存在时脚本自己构建（`CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0`） |
| shell | `${SHELL:-/bin/sh}` 可执行 | terminal connector 要 spawn 一个 shell，缺了整条链建不起来 |

**不需要 `opencode`，也不需要任何模型凭据。**

**磁盘**：这台机器是 40 G 盘且**没有 swap**。`cargo build` 前确认有 ≥ 1 G 空闲，
并发编译会撞 OOM（症状是 `cargo` 返回 101 且 shell 完全没有输出）。
`CARGO_BUILD_JOBS=1` 是硬要求，不是建议。

**不改仓库里的 `data/`**：Hub 与设备的数据目录、以及那个临时工作区，都在本次运行的
临时目录里（`AFHUB_DATA_DIR` / `AFCLI_DATA_DIR` / `mktemp -d`）。

## 3 两个已知坑

### 坑一：断言读了一个契约里不存在的字段

`/api/devices` 的对外契约是：

```
{id, name, platform, node_version, status, last_seen, cpu, mem, gpu,
 agents, usable_agents, derived, key_fingerprint}
```

**没有 `connectors`**。能力上报在 Hub 的 `device` 表里，通道门禁读的就是它。
所以断言 1 用 `sqlite3` 直接读：

```sql
select json_extract(capabilities, '$.connectors') from device where id='<node_id>';
```

> **这里踩过 M2 与 v0.4.0 的同一个坑**：按 `.[0].capabilities.connectors` 取时，
> `jq` 在字段不存在时**静默给 `null` 且退出码 0**，于是断言永远红，而红的原因
> 看起来像「设备没上报」。实测把断言 1 改成读 `.[0]` → 立刻红，文案却是
> 「设备没上报 terminal」，完全指错了方向。

**v0.5.0 还多踩了同一形状的第二个**：最初想在断言 5 里用 `GET /api/channels`
数通道数。**那个端点在 Hub 上根本不存在**——`crates/hub-server/src/lib.rs:677`
只注册了 `POST /api/channels`。取它会 405，`jq` 对空体静默给 `null`，
于是「通道数不变」断言永远绿（`null = null`）。已改成不依赖该端点。

**判据**：写新断言前先 `grep` 一次路由注册，别假设 REST 里“应该有”的端点就有。

### 坑二：夹具 / 判据造错，导致断言永远红**或永远绿**

v0.4.0 记的是「永远红」那一种（夹具把 `HEAD` 造错了）。v0.5.0 撞上的是更危险的
**永远绿**：断言 5 最初写「日志里存在当前通道的 `channel.ready`」。

`channel.ready` 是**已经发生过**的事件，它躺在 `$SSE_LOG` 里不会消失。于是即使页面
走了「先释放再重建」，旧 id 的那一行还在，判据照样命中——**断言恒真**。

> **实测**：构造「点当前那个 = 先关再开」的变异，那种写法依然 18/18 全绿。
> 改成比对**检查前后的 `ready` 行数**（`2 → 3`）之后，同一变异立刻红。

**教训**：验「某件事没发生」的断言，判据必须是**增量**（前后的差），不能是
「日志/状态里存在某个东西」。存在性是单调的，它证明不了「刚才是安静的」。

## 4 断言清单

| # | 内容 | 对应的页面行为 |
|---|---|---|
| 1 | `device.capabilities.connectors` 含 `terminal` | 通道门禁的前提 |
| 2 | `POST /api/sessions` 201 | 进会话 |
| 3 | 开 terminal 通道 201，且 SSE 里回 `channel.ready` | 选中终端 = 建它自己那条通道（§5.5.1 懒建立） |
| 4 | 释放旧通道 204，且**旧通道再也发不进去** | 换工具 = 释放被换掉那个（§7.1.3） |
| 5 | 点当前那个工具之后：**没有新的 `channel.ready`**、通道**仍可发送** | 「＋」选回自己 = 零副作用（§7.1.2 的唯一例外） |
| 6 | 替换只释放被换掉那一个：对照通道仍可发送，被释放那条真的断了 | `openPanel` 只处理一个前值，不是「全清」 |
| 7 | 收起右侧一格 → 204、再也发不进去、重复释放仍 204 | 退出即释放（§7.2），且幂等 |

**没有「跳过」这一档。** 这里的先决条件本机必然满足，缺了就是环境坏了（退出码 2），
不该画成 ⏭️ 蒙过去。脚本也没有 `skipped()` 计数：留一个永远为 0 的计数只会让人以为
「跳过是被支持的」，而当前唯一的处置是失败。

### 断言 4 为什么要验「再也发不进去」

只验 `DELETE` 回 204 的话，一个「回了 204 但没通知设备」的实现也能过——而那样 PTY
会活着，用户丢掉的 shell 就白丢了。所以判据是**这条路真的断了**（任何非 2xx 都算数：
404 通道不存在、409 已关……它们都意味着同一件事）。实测去掉 `DELETE` 那一行之后，
**两条断言一起红**。

### 断言 6 为什么要开一条对照通道

「替换」错写成「把全部通道关掉」，在单通道用例里看不出来（本来也只有一条）。
所以先开第二条，再替换第一条，验第二条还活着；同时反向验被替换那条真的断了——
只验「对照活着」的话，一个「什么都不释放」的实现也能过。

## 5 反向验证记录（2026-10-29 实测）

**每一个断言都必须能被一次变异弄红，而且变异要先确认真的改到了文件。**

| 变异 | 结果 |
|---|---|
| 断言 4 不发 `DELETE` | ❌ 两条红（「期望 204」+「仍能 send」） |
| 断言 5 改成「先释放再重建」 | ❌「多出了新的 `channel.ready`（2 → 3）」 |
| 断言 1 改成读 `.[0].capabilities` | ❌「connectors 里没有 terminal」（指错方向，正是 §3 的坑） |

> **一个必须记下来的过程事故**：变异 C 第一次跑出来是**绿的**，原因不是断言太弱，
> 而是**我的字符串替换静默失配**（那行代码末尾有尾随空格），文件压根没变。
> 于是“跑了变异但全绿”看起来像“断言抓不到”，实际是“变异根本没发生”。
> 现在每个变异之后都先比 `sha256sum`，不等就停下。
> **反向验证本身也要被验证**——这是这一节存在的理由。

## 6 怎么跑

```bash
bash scripts/e2e-v0.5.0.sh              # 全自动跑一遍，结束即清理
bash scripts/e2e-v0.5.0.sh --keep       # 保留日志现场（临时目录路径会打出来）
bash scripts/e2e-v0.5.0.sh --port 19000 # 指定 Hub 端口
```

耗时约 10–20 秒（不含首次 `cargo build`）。**不要与 `cargo test --workspace`、
vitest、覆盖率同时跑**：这台机器没有 swap，四者并发会 OOM。

退出码：`0` 全通过；`1` 有断言失败；`2` 环境不满足（缺工具 / 构建失败 / 设备上一个
agent 都没上报）。

## 7 失败怎么读

脚本末尾打印「通过 N，失败 M」。失败行上方是 `❌`，下方是现场
（`dump()` 展开 Hub 日志 / 设备日志 / **该通道收到过的全部载荷**）。

**先看载荷再看日志。** 载荷是产品真实吐出来的东西；Hub / 设备日志只解释「为什么」。

| 症状 | 多半是 |
|---|---|
| 断言 1 红、载荷为空 | 设备没上报 `connectors`——查 `afcli` 心跳的 `capabilities` |
| 开通道回 409 | 能力硬过滤：`connectors` 里没有 `terminal`（断言 1 本该先红） |
| 断言 3 红、没有 `channel.ready` | 设备侧 PTY 起不来——查 `$TMP/node/logs/afcli.log` 里 spawn 的错误 |
| 断言 4 红、「仍能 send」 | 释放没送到设备：查 Hub 有没有发 `close_channel` |
| 断言 5 红、`ready` 行数变大 | 页面真的走了「先释放再重建」——`addPanel` 的判等丢失了 |
| `cargo` 返回 101 且无输出 | 磁盘满 / OOM（见 §2） |

`--keep` 会打出临时目录路径，里面有 `hub.log` / `sse.log` / `node/logs/afcli.log`。
