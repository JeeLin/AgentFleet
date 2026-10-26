# AgentFleet

个人自托管的设备 / Agent 平台 / Personal Self-Hosted Device & Agent Platform

把自己拥有的每一台设备——服务器、电脑、手机、树莓派——注册为节点；
在一个 Web 页面里统一管理，在节点上打开终端与文件、运行 AI agent 会话，
并按设备能力把任务分发到最合适的节点执行。

Turn every device you own—servers, PCs, phones, Raspberry Pis—into a node of one
self-hosted fleet: manage them all in one web console, open terminals and files on any
node, run AI agent sessions, and route tasks to the node best suited to run them.

> **单用户 · 自托管 · 数据自主掌控 · 零入站端口 · 深色优先**
> **Single-user · Self-hosted · Data sovereignty · Zero inbound ports · Dark-mode first**

> **Hub 不会主动拨入你的任何设备**：一条命令接入，节点自己外联。
> **The Hub never dials into your devices**: enroll with one command, nodes dial out.

---

## 为什么 / Why

- **一个界面管全部设备**，不必逐台 SSH。**One console for every device**—no per-device SSH.
- **公网与内网同一个服务**，节点反向外联，**零端口暴露**。**One service** for public and LAN, nodes dial out, **zero port exposure**.
- **数据自主掌控**，完全**自托管**。**Data sovereignty**—fully **self-hosted**.
- **任务按设备能力分发**，交给最合适的节点。**Tasks routed by capability** to the best-fit node.

---

## 第一版范围 / First Release Scope

> 「第一版」指**首个可用版本（里程碑 M0–M4）**。M5 起为第二阶段。
> "First release" = the first usable version (M0–M4). M5 onward is the next stage.

> **当前进度 / Current status：M0、M1、M2 已完成，M3 起未开始。**
> M1 交付的是**会话容器**（页面级：工作区 + 终端 / 文件 / exec 面板）与 Web 控制台。
> M2 把 ACP 接进来：设备上报 agent 运行时、Hub 下发安装、选设备上的 agent 对话、轨迹落库。
> 下面每项后面的 `Mx` 是它真正落地的里程碑——**清单描述的是第一版全貌，不是现在就能用的东西**。
> Shipped today: M0, M1 and M2. The `Mx` tag on each line is the milestone that actually delivers it.

- **设备接入** `M0`：一条 CLI 命令注册，单条 WebSocket 主动外联；心跳即能力上报。One CLI command, one outbound WebSocket, heartbeats carry capabilities.
- **节点身份与状态** `M0`：Hub 签发 `node_id`，设备密钥对认证，不用静态共享 token。Hub-issued `node_id`, key-pair auth, no static shared token.
- **Web 控制台与会话面板** `M1`：设备 / 会话列表与工作台；终端（PTY，含 resize）/ 文件浏览 / 一次性 exec，全部绑定会话工作区 `(node_id, path)`，**零入站端口**。Devices and sessions, a workbench with terminal (PTY, resizable), file browser and one-shot exec—all bound to the session workspace, **zero inbound ports**.
- **AI agent 管理与对话** `M2`：各类 agent 统一配置、启动、对话。Configure, launch and chat with any agent in one place.
- **对话与动作流** `M2`：dsh 式 Trajectory 逐条可查，列出本次改动的**文件**与 diff。A dsh-style trajectory, step by step, plus changed files and diffs.
- **会话内 git 面板** `M3`：完整面板集补齐第三个（终端 / 文件 / git）。The third panel (git) completes the set.
- **Hub 一键安装 agent** `M2`：在 Hub 选设备 + 选 agent 模板 → 节点本地安装 → 心跳回报就绪。**必须用户手动选，不自动安装**。Pick a device and template on the Hub, the node installs it locally, a heartbeat reports readiness. **Always user-initiated.**

> **2026-10-26 范围裁定**：原第一版里的「工作区供料与统一配置」与「产物归档」**整体撤销**——
> 产品当前只做**每台设备内自己的对话**，不做任务分发，因此没有项目文件传输的需求。
> 相关的 CAS / manifest 差分 / A-B-C-D 四档传输规格留在 `docs/architecture/workspace-and-sync.md`
> 作决策记录（相关小节已标「已撤销」）。
>
> **Scope decision, 2026-10-26**: the former "workspace feeding / unified config" and
> "artifact archiving" items are **retired** — the product only does per-device conversations,
> so there is no project-file transfer surface. The CAS / manifest-diff / four-tier specs remain
> in `docs/architecture/workspace-and-sync.md` as a decision record (marked as retired).

> 完整范围与用户流程见 `docs/PRODUCT.md` §3。
> Full scope and user flows: `docs/PRODUCT.md` §3.

---

## 设计基调 / Design Tone

- **深色优先、等宽字体、高信息密度、键盘优先**。Dark-mode first, monospace, high density, keyboard-oriented.
- **终端 / 文件 / git 是会话内的面板**，易用即可。**Panels inside a session**—usable is enough.
- **对话与动作流对标 dsh**：逐条可查，改动可见。Borrow dsh: every step inspectable, every change visible.

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
mise run cli         # 开发用：debug 构建，数据落在 data/node
mise run cli-log -- -n 50
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

---

## 文档 / Documentation

- `docs/PRODUCT.md` — 产品功能边界、用户可见流程、术语表
- `docs/DEVELOPMENT.md` — 技术栈、crate 结构、里程碑总览
- `docs/architecture/` — 架构决策目录
- `AGENTS.md` — 项目约束与协作约定
- `CHANGELOG.md` — 变更日志
- `docs/BUGS.md` — 缺陷池

---

## 许可 / License

Apache-2.0 / Apache-2.0.

---

## 贡献 / Contributing

贡献前请阅读 `AGENTS.md` 的项目约束与协作约定。
Read the project constraints and collaboration conventions in `AGENTS.md` before contributing.
