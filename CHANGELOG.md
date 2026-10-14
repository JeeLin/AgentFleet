# Changelog / 变更日志

本文件遵循 [Keep a Changelog](https://keepachangelog.com/) 格式，条目为中英双语。
Entries follow Keep a Changelog; each entry is bilingual (中文 / English).

## [Unreleased]

### Added
- 产品与技术文档基线：定位、硬性约束、架构决策（会话与轨迹、任务连贯性、分布式推理）、里程碑规划 / Initial product & architecture docs: positioning, hard constraints, architecture decisions (session & trajectory, task continuity, distributed inference), milestone plan
- 架构决策：工作区供料与同步——Hub 内容寻址仓库统一供料项目文件 / 技能配置 / 产物；git 项目只传 commit 引用、非 git 走 manifest 差分；配置走托管覆盖层（只读 + 漂移检测） / Architecture decision: workspace feeding & sync — one Hub content-addressed store serves project files, skill/agent config, and artifacts; git projects transfer only a commit ref while non-git trees sync by manifest diff; config lands as a managed read-only overlay with drift detection
- 新增里程碑 **M2 工作区供料与同步**，排在 ACP 与 Git 之前 / New milestone **M2: workspace feeding & sync**, placed before ACP and Git

### Changed
- 终端 / 文件 / git 改为**会话内面板**，绑定会话工作区 `(node_id, path)`，生命周期随会话；去掉「对标 Xshell / Xftp」的设计基调 / Terminal, files, and git are now **session-scoped panels** bound to the session workspace `(node_id, path)` with session lifetime; dropped the "benchmark Xshell / Xftp" design stance
- 首个演示（M1）改为「在树莓派上开会话，会话内 shell 与文件浏览可用」 / First demo (M1) is now "open a session on the Raspberry Pi and use the shell and file browser inside it"
- 前端定选 **Vue 3 + Bun + Vite**（`packages/web`），补齐 `bun run` 质量门禁 / Frontend settled on **Vue 3 + Bun + Vite** (`packages/web`) with `bun run` quality gates
- 清空仓库内容，从零重建文档 / Cleared the repository and rebuilt the docs from scratch