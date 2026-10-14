# Changelog / 变更日志

本文件遵循 [Keep a Changelog](https://keepachangelog.com/) 格式，条目为中英双语。
Entries follow Keep a Changelog; each entry is bilingual (中文 / English).

## [Unreleased]

### Added
- 产品与技术文档基线：定位、硬性约束、架构决策（会话与轨迹、任务连贯性、分布式推理）、里程碑规划 / Initial product & architecture docs: positioning, hard constraints, architecture decisions (session & trajectory, task continuity, distributed inference), milestone plan
- 架构决策：安全与生命周期——凭据三类分层（设备密钥对 node 本地生成 / connector 凭据与模型 key 由 Hub 明文存储）、supervisor/worker 两层设备模型、掉线指数退避重连 / Architecture decision: security & lifecycle — three credential classes, a two-tier supervisor/worker device model, and exponential-backoff reconnect
- 许可定为 **Apache-2.0** / License settled as **Apache-2.0**
- 缺陷池录入三条 TODO：`secret` 表明文存储的加固项、分布式推理路线选型（结论未定）、工作区同步 B/D 档兜底 / Three TODOs entered in the defect pool: encryption hardening, distributed-inference route, sync fallbacks
- README 改为门面式：范围/技术选型细节链向 `docs/PRODUCT.md` 与 `docs/DEVELOPMENT.md`，新增文档索引 / README is now a facade: scope and stack link out to the product and development docs, plus a documentation index
- 架构决策：工作区供料与同步——Hub 内容寻址仓库统一供料项目文件 / 技能配置 / 产物；git 项目只传 commit 引用、非 git 走 manifest 差分；配置走托管覆盖层（只读 + 漂移检测） / Architecture decision: workspace feeding & sync — one Hub content-addressed store serves project files, skill/agent config, and artifacts; git projects transfer only a commit ref while non-git trees sync by manifest diff; config lands as a managed read-only overlay with drift detection
- 新增里程碑 **M2 工作区供料与同步**，排在 ACP 与 Git 之前 / New milestone **M2: workspace feeding & sync**, placed before ACP and Git

### Changed
- **connector 只面向 Web 页面用户，不向 agent 暴露**——本地 agent 完全依赖设备上安装的 agent 自带能力与它自己的权限系统；「工作区根内」只是页面侧的约束 / **Connectors serve the web UI only, never agents.** Local agents rely entirely on their own installed tooling and permission system; "confined to the workspace root" is a UI-side constraint only
- agent 的权限约束改为**两层**（Hub 全局规则 + 会话级加严，后者不可放松全局 deny），由托管覆盖层统一下发，不自研权限引擎 / Agent permissions are now two layers (Hub-global rules plus per-session tightening that cannot relax a global deny), delivered through the managed overlay; no permission engine of our own
- 子任务工作区的内容来源在**分发时由用户确认**：`HEAD`（默认）/ `HEAD` + 工作区未提交改动 / CAS `snapshot` / Sub-task workspace source is confirmed at dispatch time: `HEAD` (default), `HEAD` plus uncommitted changes, or a CAS `snapshot`
- **Hub 不做「一键安装 agent 到新机器」**：分发只覆盖配置，不覆盖 agent 软件本体 / **No one-click agent install**: distribution covers configuration only, not the agent binary itself
- **修正进程模型图**：浏览器从不直连 node，只有「浏览器 ↔ Hub」与「node → Hub」两条链路；此前图示画成浏览器直连节点，与零入站承诺冲突 / **Fixed the process-model diagram**: the browser never connects to a node directly; only "browser ↔ Hub" and "node → Hub" exist. The previous diagram implied browser→node connections, contradicting the zero-inbound promise
- 补齐轨迹事件表（类型 / 输入 / 输出 / 耗时 / 归属节点落成列），修复指向任务连贯性文档的断链 / Added the trajectory event table (type, input, output, duration, owning node as real columns); fixed the broken cross-reference
- 澄清「节点级可恢复」边界：同节点可经 ACP `session/load` 恢复，换设备则标记需重建；补断线重连与通道重建规则 / Clarified the "node-level recoverable" boundary and added reconnect / channel-rebuild rules
- 明确**不做设备间 P2P 直传**，跨节点取文件一律经 Hub 内容寻址仓库 / Made it explicit: no device-to-device P2P transfer; cross-node file access always goes through the Hub CAS
- 统一「第一版」口径 = 里程碑 M0–M4（子 agent 从「后续」移入第一版）；补齐 PRODUCT §3.1 的工作区供料 / 统一配置 / 产物归档三行 / Aligned the "first release" definition to M0–M4 (sub-agent system moved into the first release); completed the PRODUCT §3.1 scope table
- 终端 / 文件 / git 改为**会话内面板**，绑定会话工作区 `(node_id, path)`，生命周期随会话；去掉「对标 Xshell / Xftp」的设计基调 / Terminal, files, and git are now **session-scoped panels** bound to the session workspace `(node_id, path)` with session lifetime; dropped the "benchmark Xshell / Xftp" design stance
- 首个演示（M1）改为「在树莓派上开会话，会话内 shell 与文件浏览可用」 / First demo (M1) is now "open a session on the Raspberry Pi and use the shell and file browser inside it"
- 前端定选 **Vue 3 + Bun + Vite**（`packages/web`），补齐 `bun run` 质量门禁 / Frontend settled on **Vue 3 + Bun + Vite** (`packages/web`) with `bun run` quality gates
- 清空仓库内容，从零重建文档 / Cleared the repository and rebuilt the docs from scratch