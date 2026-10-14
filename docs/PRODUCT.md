# 产品文档 / Product

> 本文档定义 AgentFleet 的产品定位、功能边界与用户可见流程。架构决策见 `docs/architecture/`。
> This document defines product positioning, feature boundaries, and user-visible flows.
> Architecture decisions live in `docs/architecture/`.

---

## 1. 产品定位 / Positioning

个人自托管的**设备 / Agent 平台**。把自己拥有的全部设备（服务器、PC、手机、树莓派）
注册为节点，在一个 Web 页面里统一管理，在节点上运行 AI agent 会话，并按设备能力把任务
分发到最合适的节点执行。

第一版只做 self-host、单用户。这是范围选择，不是能力妥协。

Self-hosted, single-user **device / agent platform**. Every device you own becomes a node
of one web console; run AI agent sessions on any node and route tasks to the best-fit node.

### 1.1 非目标（显式）/ Non-goals (explicit)

- 多用户 / RBAC / 团队 / SaaS 计费
- 通用文件同步引擎（跨设备自动同步目录）
- 带 DAG 的重型调度器、消息中间件
- 异构设备张量并行推理作为第一版卖点
- 多态 Resource 抽象（connector 是有类型的、独立演进的能力）

---

## 2. 用户画像与核心价值 / Users & value

- **你有一堆设备**：服务器跑重活、PC 随手用、手机在外看进度、树莓派常驻角落。
  希望不登录每个设备、不开一堆 SSH/客户端，就能在一个界面管理和让它们干活。
- **你用各种 agent 软件**：不同厂商、不同协议的 coding agent / 通用 agent，
  希望统一接入、统一对话、统一看它们的动作。
- **你不想把数据交给 SaaS**：自托管，数据在你手中。

---

## 3. 功能边界 / Feature boundaries

### 3.1 第一版（必做）/ First release (must)

| 能力 | 用户可见行为 |
|------|------------|
| 设备接入 | `node enroll <hub-url>` 一条命令，节点生成密钥对、主动建立 WebSocket 外联；无需 Hub 入站端口 |
| 节点身份与状态 | 注册时 Hub 签发 `node_id`；心跳即能力上报（os / arch / hostname / 版本 / 支持的 connector）；在线/离线状态 |
| AI agent 管理与对话 | 接入各类 agent 软件（不同厂商/协议），在 Web 一个界面里统一配置、启动、对话 |
| 对话与动作流 | 会话有 Trajectory 视图，逐条展示 User/Assistant/Tool/子agent/压缩记录；会话结束列出本次改动文件与 diff |
| 文件浏览 | 浏览/读写节点本地目录，写操作限制在工作区根内 |
| Git 操作界面 | 节点工作区的 git 状态 / diff / 提交视图 |
| Bash / PTY | 节点进程直接提供本地 shell；不装 openssh-server，Hub 不拨入 |

### 3.2 后续（非第一版）/ Later (not first release)

- **任务系统**：完整任务的定义、状态、进度与续跑（见 §5 设计问题一）
- **自动化**：定时 / 事件触发的任务编排
- **子 agent 体系**：可配置、开箱即用的子 agent，主 agent 把单个完整任务分发到设备上执行（借鉴 oh-my-opencode）
- **connector 插件化**：把 SSH / SFTP / MySQL / PostgreSQL / Redis / SQLite / S3 等协议做成插件按需加载；额外提供**网络抓包插件**与**端口转发插件**
- **社交软件接入**：各 IM 作为 agent 的输入输出通道
- **原生客户端**：客户端本身也是一个操作本机/服务器的 agent，可包裹开源软件实现
- **分布式推理**：把接入设备作为协同的大模型运行节点（见 §5 设计问题二 与 `docs/architecture/inference.md`）
- **Hub 一键配置 agent（推后/deferred）**：在 Hub 统一管理所有设备上的 agent 配置（如统一 opencode 配置）。因「跨设备下发 + 安全回滚」较复杂，本期不做

### 3.3 用户可见流程（第一版）/ Key flows (first release)

1. **接入一台设备**：在设备上跑 `node enroll <hub-url>` → Hub 显示新节点（真实 os/arch/hostname）→ 在线。
2. **在某个设备上开 agent 对话**：选节点 → 选/配 agent → 在面板里对话 → Trajectory 逐条可查。
3. **看它改了什么**：会话结束（或随时）→ 改动文件列表 → 点开看 diff。
4. **操作文件 / git / bash**：对应面板直接作用于该节点。

---

## 4. 设计对标 / Benchmark

| 模块 | 对标 |
|------|------|
| 终端 / 文件管理 | Xshell / Xftp 的操作逻辑 |
| 对话与动作流 | **dsh**：Trajectory（逐条事件）+ 会话改动文件/diff 视图 |
| 子 agent 编排 | **oh-my-opencode**：可配置、开箱即用的专家 agent 体系 |
| 管理页（设备/agent/设置） | 自有设计系统 |

**基调**：深色优先、等宽字体、高信息密度、键盘优先、一致交互范式贯穿全站。易用优先。

---

## 5. 两个关键设计问题（结论）/ Two key design questions

### 5.1 一、任务的连贯性 / Task continuity

> 同一个任务在多设备、多节点之间流转时，状态、上下文、执行进度如何保持一致、不断裂、可续跑？

**结论（第一版不实现任务系统，但先钉死模型，避免返工）：**

- **状态与上下文外置到 Hub（single source of truth）**：任务的 transcript、工具调用轨迹、
  工作区快照引用都存 Hub，节点只持有「当前执行所需的上下文切片」。节点掉线，任务状态不丢。
- **断点可恢复**：任务 = 持久化的状态机 + 事件日志；节点执行带「检查点（checkpoint）」，
  重连后从最后一个成功检查点续跑，不重做。
- **工作区/产物定位用内容寻址或稳定引用**：跨节点续跑时，需要的文件/产物通过 Hub 侧
  的存储或节点间受控传输取得，不依赖「上一台机器还在」。
- **ACP 的 `session/load` 作为对话恢复机制**：agent 会话恢复走 ACP，而非自建。
- **轨迹 append-only**：所有动作写入 dsh 式事件流，既是 UI 回放源，也是断点续跑的审计源。

**对外口径**：第一版只做到「会话/节点级可恢复」，完整的跨节点任务连贯性在任务系统里程碑做。

### 5.2 二、把接入设备作为分布式大模型节点 / Distributed inference

> 多个设备协同运行一个大模型（模型切分 / 分布式推理 / 异构组网），可行性、约束与路径？

**结论（完整评估见 `docs/architecture/inference.md`）：**

- **异构消费级设备做真实张量并行不可行**：张量并行每层多次跨设备归约，受互联带宽/延迟限制；
  llama.cpp 的 RPC 后端只做「逐算子转发 + 层切分（流水线并行）」，没有集合通信原语，
  跨机 TP 不可用；MoE 走 expert 分片（零跨节点流量）才是异构友好路径。
- **必须基于现成开源，不自研内核**（你也明确无能力审核推理内核代码）：
  - 最稳健：每台设备跑自己的全量副本 + 一个 OpenAI 兼容网关按模型/状态路由
  - 进阶：llama.cpp RPC 层切分（2-3 台强设备跑装不下的模型）
  - MoE 异构最优：mesh-llm 的 expert 分片（零跨节点流量）
  - 实验：draft 分节点投机解码
- **约束前置决策**：llama.cpp RPC 无鉴权且需入站端口，与「Hub 无入站访问」冲突——
  采用 mesh-llm 的 QUIC/Iroh 出站隧道或 rileynz 的出站 TLS 隧道模式解决。
- **对外表述**：「一个由你 N 台设备支撑的 OpenAI 兼容端点」，不是「N 台设备合成一台大模型机」。
  正确预期是「能跑更大的模型」，不是「跑得更快」。

---

## 6. 术语 / Glossary

| 词 | 含义 |
|----|------|
| device / node | 一台注册的设备及其上运行的节点进程 |
| agent | **仅指 AI agent**；绝不指设备进程 |
| Hub | 自托管的服务端，用户自己跑 |
| connector | 节点上提供的一类**有类型**的能力（terminal / files / git / exec / ssh / db / …），可插件化 |
| task | 一个完整任务；sub-task 由子 agent 在节点上执行 |
| trajectory | dsh 式的会话动作事件流，Web 逐条回放 |

---

## 7. 设计审查配置 / Review config

- 人工复核：可选（自动设计审查通过后，有争议时才进入人工复核）
