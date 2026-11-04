# v0.4.0 真设备端到端验收（git 面板）

对应里程碑 `.mdflow/milestones/v0.4.0-git-panel.md` 的子任务 6。脚本是
`scripts/e2e-v0.4.0.sh`。

**这份 README 的存在理由**：M1 的教训是主线一度没读 `scripts/e2e-m1/README.md` §1
就下了「本机跑不了」的错误结论；M2 的教训是**脚本读了一个契约里不存在的字段**，
于是断言永远红、红的原因看起来像产品坏了。v0.4.0 把这两类坑都写在这里，而不是
留在提交消息里。

## 1 它验的是什么

起**真** `afhub`、**真** `afcli`（supervisor + worker 两个进程），在一个**真 git 仓库**上
按页面走的那条路逐条断言：

| 段 | 链路 |
|---|---|
| 页面 → Hub | `POST /api/channels`（开 `connector:"git"` 通道）+ `POST /api/channels/{id}/send` |
| Hub → 页面 | `GET /api/stream`（SSE：`channel.frame` / `channel.ready`） |
| Hub ↔ 设备 | `/ws`（协议帧，由真 afcli 自己维持） |
| 设备 → git | `node-core` 的 git connector 亲自 spawn `git` 子进程 |

这条链路上**没有一处是替身**。断言看的是 SSE 里真实收到的 `channel.frame` 载荷，
而不是脚本自己调 `git` 得出的值——后者只能证明 git 好使，证明不了产品好使。

**与 M2 的关键区别：不需要 agent。** git connector 与 agent 无关。但 `POST /api/sessions`
**要求 `agent_id` 必填**（M2 起，缺了是 400 `agent_required`），所以脚本从
`agent_runtime` 里取一个已装的名字填进去——它不参与任何断言，只为了让会话建起来。
取不到时报错退出（那不是 git 的问题，是环境不完整）。

## 2 先决条件

**不需要额外硬件、不需要浏览器、不需要树莓派**（`scripts/e2e-m1/README.md` §1 已明写）。

| 项 | 要求 | 为什么 |
|---|---|---|
| 工具 | `curl` `jq` `ss` `sqlite3` `awk` `sed` `git` | 断言 1 要 `sqlite3`，端口探测要 `ss`，仓库夹具要 `git` |
| 仓库 | 能 `cargo build --bin afhub --bin afcli` | 二进制不存在时脚本自己构建（`CARGO_BUILD_JOBS=1`） |
| `git` 可用 | `command -v git` 有输出 | 夹具要真建仓库、真提交 |

**不需要 `opencode`，也不需要任何模型凭据。**

**磁盘**：这台机器是 40 G 盘。`cargo build` 前确认有 ≥ 1 G 空闲，否则会撞
`No space left on device`（症状是 `cargo` 返回 101 且 shell 完全没有输出）。
`rm -rf target/debug/incremental` 通常能回约 1 G。

**不改仓库里的 `data/`**：Hub 与设备的数据目录、以及被观察的 git 仓库，都在本次
运行的临时目录里（`AFHUB_DATA_DIR` / `AFCLI_DATA_DIR` / `mktemp -d`）。
提交者身份用 `git -c user.name=… -c user.email=…` 传给**每条命令**，不写用户的全局
`git config`。

## 3 仓库夹具为什么要造成那样

脚本造的仓库有**两条提交**和一个有未提交改动的工作区：

| 状态 | 用来验 |
|---|---|
| 首次提交：`edited.txt` + `first.txt` | 仓库不是空的（空仓库的 log 是另一条路径） |
| **HEAD**：只改 `first.txt` | 断言 6 的「该提交的 diff」——它只含 `first.txt` |
| 工作区：`edited.txt` 改了未提交 | 断言 3 的 ` M` 状态码、断言 4 的工作区 diff |
| 工作区：`untracked.txt` 未跟踪 | 断言 3 的 `??` 状态码 |

**HEAD 必须是「只改 first.txt」那条**，因为断言 6 要点它，并检查三件事：
它的 diff 含 `second commit content`、**不含**工作区的 `line two added`、
**不含** `edited.txt` 的文件头。后两条才是抓「取错基线」的那一刀。

> **这个夹具曾经造错过（2026-10-29 实测）**：第一版把二次提交写成「先加两个文件、
> 再改 first.txt」，于是 `HEAD~1` 是**首次提交**——它压根不含 `second commit content`，
> 断言 6 永远红。红的原因看起来像「产品取错基线」，实际是「夹具把顺序搞反了」。
> 排障时 SSE 里那份 `{"commit":"adcb05e…","op":"diff",…}` 立刻证明了产品是对的。
> 教训：**夹具的期望值要能从夹具本身直接读出来**，不能靠「我觉得 HEAD~1 应该是那样」。

## 4 断言清单

| # | 内容 | 档 |
|---|---|---|
| 1 | `device.capabilities.connectors` 含 `git`（通道门禁的前提） | 自动 |
| 2 | `POST /api/sessions` 201 | 自动 |
| 3 | 开 git 通道 201；`status` 的 `branch` 与仓库真实分支一致、`clean=false`、两列状态码原样（` M` / `??`） | 自动 |
| 4 | `diff` 的 `path` 档：含该文件的新增行、**不含**已提交内容、`truncated=false` | 自动 |
| 5 | `log` 档：回 ≥ 2 条、第一条是最新提交（新→旧）、`truncated=false` | 自动 |
| 6 | `diff` 的 `commit` 档：含该提交改过的文件、**不含**工作区改动、**不含**别的提交的文件 | 自动 |
| 7 | 非仓库目录：`status` 回**正常帧**且 `not_a_repo=true` / `clean=null` / `files=[]`，**不落到 `channel_error`** | 自动 |

**没有「跳过」这一档。** 这里的先决条件（`git` + 一个临时仓库）本机必然满足，
缺了就是环境坏了（退出码 2），不该画成 ⏭️ 蒙过去。脚本也没有 `skipped()` 计数：
留一个永远为 0 的计数只会让人以为「跳过是被支持的」，而当前唯一的处置是失败。

## 5 断言 1 为什么不读 `/api/devices`

`/api/devices` 的对外契约是：

```
{id, name, platform, node_version, status, last_seen, cpu, mem, gpu,
 agents, usable_agents, derived, key_fingerprint}
```

**没有 `connectors`**。它只给「已装 agent」与「已装且可用 agent」——那是设备页要的。
能力上报（`capabilities`）留在 Hub 的 `device` 表里，通道门禁读的就是它。

所以断言 1 用 `sqlite3` 直接读：

```sql
select json_extract(capabilities, '$.connectors') from device where id='<node_id>';
```

> **这里踩过 M2 同一个坑（2026-10-29 实测）**：第一版按
> `.[0].capabilities.connectors` 取，`jq` 在字段不存在时静默给 `null`，
> 于是断言永远红，而红的原因看起来像「设备没上报 git」。
> 记录在 `scripts/e2e-m2/README.md` §3（那次是 `.[].version`）。

## 6 断言 7 为什么只有真设备能验

`packages/web/src/lib/channel.ts` 的 `errorCodeOf()` **只保留错误码、丢弃 `reason`**。
所以「不是 git 仓库」若走错误路径，页面拿到的只是一个 code（`unsupported` / `internal`），
**分不清「不是仓库」和「git 真的挂了」**——也就画不出中性说明。

设备侧因此把它改成**正常回帧**（`crates/node-connector/src/connector/git.rs` 的
`is_not_a_repo` 分支）：`{"op":"status","not_a_repo":true,"clean":null,…}`。
断言 7 验的就是这件事，且**在两个层次上验**：

1. 帧内容：`not_a_repo=true`、`clean=null`（不是 `true`——那会让页面写「没有未提交的改动」）；
2. 路径：这条通道上**不该**出现 `channel_error`。只验 1 的话，一个「既回正常帧又回
   `channel_error`」的实现也能过，而那会让页面同时显示两句话。

## 7 怎么跑

```bash
bash scripts/e2e-v0.4.0.sh              # 全自动跑一遍，结束即清理
bash scripts/e2e-v0.4.0.sh --keep       # 保留日志现场（临时目录路径会打出来）
bash scripts/e2e-v0.4.0.sh --port 19000 # 指定 Hub 端口
```

退出码：`0` 全通过；`1` 有断言失败；`2` 环境不满足（缺工具 / 构建失败 / 设备上一个
agent 都没上报）。

## 8 失败怎么读

脚本末尾打印「通过 N，失败 M」。失败行上方是 `❌`，下方是现场
（`dump()` 展开 Hub 日志 / 设备日志 / **该通道收到过的全部载荷**）。

**先看载荷再看日志。** 载荷是产品真实吐出来的东西；Hub / 设备日志只解释「为什么」。

| 症状 | 多半是 |
|---|---|
| 断言 1 红、载荷为空 | 设备没上报 `connectors`——查 `afcli` 心跳的 `capabilities` |
| 开通道回 409 | 能力硬过滤：`connectors` 里没有 `git`（断言 1 本该先红） |
| 断言 6 红、载荷里有 `{"commit":"…"}` | 夹具期望值写错了（见 §3），先核对 `HEAD` 到底改了哪个文件 |
| 断言 7 红、载荷里只有 `channel_error` | 非仓库又走回错误路径了（见 §6） |
| `cargo` 返回 101 且无输出 | 磁盘满（见 §2） |

`--keep` 会打出临时目录路径，里面有 `hub.log` / `sse.log` / `node/logs/afcli.log` /
被观察的仓库本身。
