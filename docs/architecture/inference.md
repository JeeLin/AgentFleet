# 分布式推理评估 / Distributed Inference Evaluation

> 结论先行：异构消费级设备做**真实张量并行不可行**；可行路径由稳到险是
> 「每设备全量副本 + 网关路由」→「llama.cpp RPC 层切分」→「MoE expert 分片」→「draft 投机」。
> 全部**基于现成开源项目**不自研内核。对外是「一个由 N 台设备支撑的 OpenAI 兼容端点」，
> 不是「N 台设备合成一台大模型机」。正确预期是「能跑更大的模型」，不是「跑得更快」。
>
> Bottom line: real tensor parallelism across heterogeneous consumer devices is infeasible.
> Viable paths run from per-device full replicas + gateway routing, through llama.cpp RPC
> layer-splitting, to MoE expert sharding and (experimental) speculative draft offload.
> All built on existing open-source projects, not a self-built kernel.

调研日期 2026-10-14，事实基于开源仓库与官方文档；推断处已标注。

---

## 1. llama.cpp：当前唯一成熟的跨机路径

**版本机制**：llama.cpp 用滚动 build 号（最新 `b11588`），不是语义版本。**全节点必须同一 build**——
GGML wire format 在 release 间会变，版本不一致会在握手 hang 或推理中崩溃。这是自托管必须自己 pin 的约束。

**`--rpc` 能做什么**：RPC 后端把每个张量算子序列化后发到远端 `ggml-rpc-server` 执行，
远端像「一块有内存、能跑算子的设备」。能力：
- 多机权重 + KV cache 池化，跑单机装不下的模型
- 混合后端（Metal + CUDA + CPU）在同一次推理中共存
- 默认按可用内存比例分配权重/KV，`--tensor-split` 可覆盖
- RDMA（Linux RoCEv2 / macOS TB5），不支持回退 TCP

**关键限制**：
- **无认证、无加密**——官方 README：proof-of-concept，"fragile and insecure"。
  鉴权/加密必须由平台隧道层提供，绝不直接暴露 RPC 端口
- **启动时静态列节点 IP**：无服务发现、无故障摘除、无自动重连；节点掉线即推理崩溃
- 逐算子往返，**瓶颈是延迟不是带宽**（社区实测 RPC 带宽占用 <20MB/s，
  但 3×Threadripper 在 1GbE 上只有 1–5 tok/s）

**张量并行？** llama.cpp 有 `--split-mode tensor`，但官方标 EXPERIMENTAL、
KV cache 必须非量化、依赖后端集合通信原语（NCCL/SYCL）。**RPC 后端没实现这套集合通信**，
只能逐算子转发——因此**跨机 RPC 上 TP 不可用**，`--rpc` 走的是**流水线并行（层切分）**。
TP 的 MoE 架构还基本全不支持（Grok/DeepSeek2/OLMoE/Mixtral 系…）。

**MoE over RPC（层切分）可用**：Qwen3-235B-A22B（Q4≈132GB）跨 Metal+CUDA 等有社区实测。

---

## 2. 其他方案的现状

| 项目 | 状态 | 结论 |
|------|------|------|
| **exo** | **活跃**（非停更），v1.0.71；**Tier 1 只有 Apple Silicon Mac**，Linux 仅 CPU；TB5 RDMA 需 macOS 26.2+ 且全互联 | **不适用**——你的设备组合（x86 服务器 + 树莓派 + 手机）无一在 Tier 1 |
| **Petals** | last push 2024-09，release 停在 2023-09（v2.2.0），模型停在 2023–2024 | **不适用**——休眠多年，设计目标是公网志愿者网格而非受控局域网 |
| **vLLM / SGLang** | 活跃 | **不适用**——多机 TP/PP 必须同构加速器 + Ray/高速互联；「支持异构硬件」指的是不同厂商各自独立部署，不是混在一个模型实例 |
| **llm-d / Ray+vLLM / dstack** | 活跃 | **不适用/仅部分**——需完整 K8S/容器集群；dstack 是作业调度器，不提供跨机模型切分 |
| **DistServe** | 休眠（last push 2025-04），研究原型，只支持 GPT-2/OPT/Llama2 | **不适用** |
| **PowerInfer / LLM in a Flash** | PowerInfer 半活跃（重心转商业硬件） | **正交**——单机内存层级优化，不解决多机组网 |

---

## 3. 面向个人多设备的新项目（最值得关注）

- **mesh-llm**（Apache-2.0，活跃，700+ stars）：稠密模型走流水线并行（轻度 fork llama.cpp + RPC），
  **MoE 走 expert 分片且推理时零跨节点流量**。三个关键优势直击本项目痛点：
  - QUIC 端到端加密（Iroh relay）——**解决 llama.cpp RPC 无鉴权**
  - 每个节点本地有 OpenAI 兼容端点；HTTP 流式对延迟容忍，而 RPC 延迟是乘性的
  - 有 mobile chat 路线图、Apache-2.0 可商用
- **prima.cpp**（MIT，新）：**异构感知调度**的 llama.cpp 分布式 fork——按每台设备的
  算力/磁盘/内存/OS 决定分多少层，多少放 GPU。明确支持 **Android / HarmonyOS**，
  是唯一把手机纳入设计目标、有论文支撑的实现。可吸收其调度算法。
- **distributed-llama**（MIT，3k stars）：**真张量并行**、通信效率高于 RPC，
  但**节点数必须 2 的幂**、最大节点数 = KV head 数，对「有什么用什么」不友好。
- **rileynz/Distributed-LLM**：控制面套官方 RPC，worker 只绑 `127.0.0.1`，
  通过**出站 TLS 隧道**复用——**同时满足「无入站」与「鉴权」**，架构上最值得借鉴。
- **SharedLLM**（商业 AGPL）：worker 发现 + 自动切分 + HMAC 认证代理；但 AGPL 有传染性。

---

## 4. 可行路径（按稳健 → 激进）

| # | 路径 | 适用场景 | 硬约束 | 风险 |
|---|------|---------|--------|------|
| 1 | **每设备全量副本 + OpenAI 兼容网关** | 异构、网络差（含跨互联网）都可用；手机/树莓派作为独立小模型节点 | **无**。只走 HTTP，对延迟不敏感 | 低。跑不了超单机内存的模型 |
| 2 | **llama.cpp RPC 层切分** | 2–3 台最强设备跑装不下的模型 | 同一 build；受信 LAN 或隧道（无鉴权）；LAN RTT <1ms；**decode 会劣化** | 中。无服务发现/故障摘除，需自己封装 |
| 3 | **MoE expert 分片**（mesh-llm 模式） | MoE 模型 + 异构设备 | 推理零跨节点流量→对网络最友好；每节点要放完整 trunk | 中。依赖 mesh-llm 单一实现 |
| 4 | **draft 分节点投机解码** | 强弱分明设备 | draft 必须显著快于 target | 中高。**未验证** |
| 5 | **distributed-llama 张量并行** | 节点算力对等 | 节点数 2^n；上限 = KV head 数 | 高。单人维护 |

**明确排除**：vLLM / SGLang / llm-d / Ray+vLLM / exo / Petals / DistServe / 跨机张量并行（RPC 上不可用）。

---

## 5. 推荐架构（分阶段）

### 核心矛盾与前置决策

平台的硬约束是「节点主动外联、Hub 无入站端口」，而 llama.cpp RPC 原生要 `bind 0.0.0.0`（入站）。
**必须在架构层面解决**，两条路：
1. mesh-llm 的 QUIC + Iroh relay（端到端加密、设备主动外联）
2. rileynz 的出站 TLS 隧道（worker 只绑 127.0.0.1，复用到 Hub/coordinator）

建议借鉴**方案 2 的出站隧道模式**（同时满足无入站 + 补上 RPC 无鉴权）。

### 阶段 0 —— 单机基线

在最强 x86 服务器跑 llama.cpp（pin 一个 build），建立 tok/s、TTFT、内存基线。
后续每阶段都要能回答「比这个快/能装更大模型吗」。单机够用就到此为止——分布式只会更慢。

### 阶段 1 —— 请求级路由（最稳健，优先做）

每台设备独立跑 llama-server，装它装得下的模型（服务器 30B+，树莓派/手机 1–3B）。
Hub 做：设备注册（沿用设备密钥对）、健康探测 + 能力上报（可用内存/支持模型/当前负载）、
按 `model` 路由、模型分发（SHA-256 校验）。完全符合「节点主动外联」，手机树莓派立刻有用。

### 阶段 2 —— 内存池化（RPC 层切分）

对 2–3 台强设备启用 RPC 层切分跑装不下的模型。工程要点：
- Hub 作 coordinator：自动发现、按可用内存算 `--tensor-split`、管 `--rpc` 列表、掉线降级回阶段 1
- **强制全网同 build**（Hub 保证）
- 出站隧道，不暴露 RPC 端口
- 远端开 `-c` tensor cache 加速重复加载
- **树莓派/手机不进 RPC 组**（CPU 慢会拖垮流水线；手机系统会挂起/热节流进程），留在阶段 1

### 阶段 3 —— MoE expert 分片（可选）

若目标模型是 MoE，引入 expert 分片（每节点独立 llama-server、零跨节点流量、会话按 hash 路由）——
异构设备跑 MoE 的最优解，把网络延迟从逐 token 路径彻底移除。

### 阶段 4 —— 投机解码（可选，实验）

draft 固定在一台快的本地设备，target 跨机。**需先实测**（文档未覆盖此用法）。

---

## 6. 平台侧职责（不自研推理内核）

Hub 只做编排：设备管理、build 号钉死、内存探测、切分计算、模型分发、请求路由、故障降级。
推理全部交给 llama.cpp 官方二进制或 mesh-llm 等现成实现。

**性能预期管理**：跨机组网的正确预期是「能跑更大的模型」。产品文案与 UI **不应承诺加速**。