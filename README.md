# AgentFleet

个人自托管的项目平台 / Personal Self-Hosted Project Platform

你建一个**项目**，项目下面有**任务**，任务被派到**设备**上用 **agent** 执行，产出的代码回到项目。
设备是你自己的——服务器、电脑、树莓派——注册成节点，**主动外联，零入站端口**。

Create a **project**, put **tasks** on its board, and let them run on **agents** on your own
**devices**—servers, PCs, Raspberry Pis—each dialing **out** to the Hub. **No inbound ports.**

> **单用户 · 自托管 · 数据自主掌控 · 零入站端口 · 深色优先**
> **Single-user · Self-hosted · Data sovereignty · Zero inbound ports · Dark-mode first**

> **Hub 不会主动拨入你的任何设备**：一条命令接入，节点自己外联。
> **The Hub never dials into your devices**: enroll with one command, nodes dial out.

---

## 为什么 / Why

- **做事从项目进，管理从设备进**。项目下面有看板、管家、历史；设备只是执行面。
  **Work starts from a project**; devices are the execution surface, not the entry point.
- **公网与内网同一个服务**，节点反向外联，**零端口暴露**。
  **One service** for public and LAN, nodes dial out, **zero port exposure**.
- **数据自主掌控**，完全**自托管**。**Data sovereignty**—fully **self-hosted**.
- **任务按项目约束 + 设备能力分发**，交给真正能跑的节点；**跑不了就停在待办并说明原因，不降级凑数**。
  **Tasks routed** by project constraints then device capability—and **parked with a reason**
  rather than dispatched to a node that can't do it.

> **产品主轴是「项目」，不是「设备 / agent」。** 设备与 agent 管理保留，但它是**管理面**。
> 「设备 / agent 平台」的旧定位已作废，理由见 `docs/DECISIONS.md` D17。

---

## 产品形状 / Shape

```text
Project ──┬── Butler（管家：接意图 / 建派任务 / 督进度 / 写总结）
          ├── Task 看板（默认视图）
          └── Binding ──→ Device ──→ Agent
```

- **看板是默认视图**：进项目先看到任务，不是仓库浏览器。
- **任务六态** `pending / queued / running / review / done / failed`。
  **失败必须留「为什么」**，停在失败列不自动回待办；**重试必须换条件**（换设备 / 换 agent / 换起点）。
- **项目三形态**：中心式（Hub 持有仓库）/ 隔离式（代码不出设备）/ 镜像式。
- **外部 issue 只做单向导入导出**；Hub 内用**收件箱**收待决策的事，不用你去看 issue。

> 完整范围与用户流程见 `docs/PRODUCT.md` §3。

---

## 设计基调 / Design Tone

- **深色优先、等宽字体、高信息密度、键盘优先**。Dark-mode first, monospace, high density, keyboard-oriented.
- **终端 / 文件 / git 是会话内的面板**，进会话才建立、退出即释放。**Panels inside a session.**
- **不编进度百分比**：agent 不上报进度，报**时长 + 最后一次动作 + 是否卡住**。
  **No invented progress bars**—report elapsed time, last action, and whether it's stuck.

---

## 技术选型 / Stack

Rust 单二进制（Hub + node）、SQLite（WAL）、ACP 会话层、append-only 轨迹、开源推理内核、Vue 3 + Bun。
Rust single binaries (Hub + node), SQLite (WAL), ACP sessions, append-only trajectories, open-source inference kernels, Vue 3 + Bun.
详见 `docs/DEVELOPMENT.md` §1 / See `docs/DEVELOPMENT.md` §1.

---

## 跑起来 / Running It

```bash
mise install          # node 24 是硬需求（Vite 7 + plugin-vue 用到 node:crypto 的 hash）

# 开发：Hub（8080 / 仅回环）+ 前端（3002 / 热更新），前端把 /api 与 /ws 转发给 Hub
mise run dev

# 同一台机器上起一台设备（debug 构建，连上面那个 Hub）
mise run cli                    # 另一个终端里跑；改 crates/ 即重编重启；设备数据落在 data/node
mise run cli-log -- -n 50       # 看设备日志（一次性命令，不经 watch）

# 生产：前端先编译、产物嵌进二进制，然后单进程单端口
mise run build && mise run start

# 设备接入（在设备上跑，不是 Hub 上）
afcli run            # 前台阻塞；想脱离终端请用 systemd（见下）或 tmux
```

### 设备上跑成服务（systemd）

`afcli run` 是**前台阻塞**的：supervisor 是一个无限 loop，进程自己不会 daemon 化
（仓库里没有 setsid / nohup 路径，这是刻意的——见 `docs/DECISIONS.md` D1）。
所以「开机自启 + 崩了自动拉起」交给 systemd，用仓库里的 unit 模板：

```bash
sudo useradd --system --home /var/lib/agentfleet --create-home agentfleet
sudo install -m 0755 afcli /usr/local/bin/afcli
sudo mkdir -p /etc/agentfleet
sudo install -m 0600 deploy/afcli.env.example /etc/agentfleet/afcli.env
sudo "$EDITOR" /etc/agentfleet/afcli.env      # 至少把 AFCLI_HUB_URL 改掉
sudo install -m 0644 deploy/afcli.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now afcli
systemctl status afcli          # 崩了看 journalctl -u afcli -f
```

**两层退避不重复**：systemd 的 `Restart=always` 管的是**进程存活**（supervisor 被 OOM kill
或被打死），supervisor 内部的 `CrashStats` / `decide` 管的是**worker 生命周期**（worker 崩了
怎么退避、什么时候判它是坏版本）。少任何一层都会留一个没人管的窗口。

`AFCLI_HUB_URL` **必须显式给**：缺省是 `ws://127.0.0.1:8080/ws`，一台真设备给它等于
永远注册不上。同理 `AFCLI_DATA_DIR` 一旦定了就**不要改**——换了就等于换了一台设备
（新 `node_id`、要重新 enroll）。

首次启动 Hub 会在 stderr 打一行 setup token（同时写进 `<data>/setup-token.txt`，0600）。
打开页面 → 用 token 完成首次 setup → 设密码 → 登录。**没有默认密码**。

```bash
# Development: Hub on 8080 (loopback only) + Vite on 3002 with hot reload, proxying /api and /ws
# Production: one process, one port, the built console embedded in the binary
# Device: `mise run cli` starts afcli; its data dir is `data/node` (not the platform default)

# Device as a service: `afcli run` blocks in the foreground and never daemonises,
# so use the systemd unit in `deploy/afcli.service` (`Type=simple`, `Restart=always`)
# See README above / the Chinese section for the install commands.
```

On first start the Hub prints a setup token to stderr (also written to `<data>/setup-token.txt`,
mode 0600). Open the console, complete first-run setup with that token, then log in.
**There is no default password.**

**两侧的数据目录缺省不同，别混。**

- **Hub** 缺省落项目级 `data/`（`mise.toml` 的 `AFHUB_DATA_DIR`），不落 `~/.local/share/`。
- **设备（`afcli`）** 缺省落平台标准目录（Linux `~/.local/share/agentfleet`），
  可用 `AFCLI_DATA_DIR` 覆盖；`mise run cli` 已把它指到 `data/node`。
  不覆盖时的全局身份会被同一台机器上所有 Hub 共用。

The two sides default differently — don't conflate them.

- **Hub** defaults to the project-local `data/` (`AFHUB_DATA_DIR`), not `~/.local/share/`.
- **Device (`afcli`)** defaults to the platform data dir (Linux `~/.local/share/agentfleet`),
  overridable with `AFCLI_DATA_DIR`. `mise run cli` points it at `data/node`.
  Without the override that global identity is shared by every Hub on the machine.

> **`agent` 由用户自己装，Hub 不代装**，只扫描节点上装了什么并保存启动配置——
> 让 Hub 往设备写可执行文件等于「一处配置错、全体设备执行未核对代码」。
> **Agents are installed by you on the device**; the Hub only scans and stores launch config.

---

## 现在到哪一步了 / Where It Stands

v0.1.0–v0.6.0 已完成（设备接入 → 会话容器 → ACP 对话 → git 面板 → 缺陷清算 → 执行起点与产出概览）。
下一步是把这些挂到**项目**这个主轴上（Task 看板、多执行、管家、收件箱）——
**这是重挂载，不是重写**：已有的会话、轨迹、worktree 机制是底座，差的只是归属关系。
里程碑见 `docs/DEVELOPMENT.md` §3。
Shipped: v0.1.0–v0.6.0. Next: remounting them onto projects (board, executions, butler, inbox)—
a **re-hang**, not a rewrite.

---

## 文档 / Documentation

| 想知道 | 去哪 |
|---|---|
| 产品要做什么、不做什么 | `docs/PRODUCT.md` |
| 为什么这么定、否决了什么方案 | `docs/DECISIONS.md` |
| 具体机制怎么运转 | `docs/architecture/` |
| 里程碑、技术栈、质量门禁、审查清单 | `docs/DEVELOPMENT.md` |
| 已知缺陷 | `docs/BUGS.md` |
| 项目约束与协作约定 | `AGENTS.md` |
| 变更日志 | `CHANGELOG.md` |

---

## 许可 / License

Apache-2.0 / Apache-2.0.

---

## 贡献 / Contributing

贡献前请阅读 `AGENTS.md` 的项目约束与协作约定。
Read the project constraints and collaboration conventions in `AGENTS.md` before contributing.
