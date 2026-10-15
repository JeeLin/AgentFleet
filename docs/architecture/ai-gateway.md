# AI Gateway 需求记录 / AI Gateway Requirements

> **状态：已定方向，待排里程碑。** 决策日 2026-10-15。
> 本文是 Gateway 的需求权威来源。实现排期见 `DEVELOPMENT.md`。
> 取代 `inference.md` 作为**推理相关的主线方向**；`inference.md` 降级为
> 「本地推理的可行性调研存档」，只在本路线要用到它的结论时才参考。
>
> Status: direction decided 2026-10-15, milestone not yet scheduled.
> This document is the requirements source of truth for the gateway.
> `inference.md` is retained only as feasibility research for *local* inference.

---

## 1. 为什么转向 / Why the change

原计划是「把接入设备作为协同的大模型运行节点」（分布式推理）。用户重新审视后认为
**应当先做 AI Gateway**，把推理这件事推后。两条理由：

1. **原计划与既有约束自相矛盾**：`AGENTS.md` 约束 7 要求「不自研推理内核，Rust 只做编排与网关」，
   但 `inference.md` 阶段 1–4 实际要 Hub 去编排分布式推理——管 `--rpc` 列表、算 `--tensor-split`、
   强制全网同 build、MoE expert 分片。这**就是**约束 7 后半句禁止的「自研分布式推理编排」。
   转向 Gateway 后，Hub 只做「选一个上游并转发」，矛盾消失。
2. **实用性**：Gateway 不需要 GPU、不需要等待设备上线，部署当天就能用；
   分布式推理需要多台强设备在线且同 build，达成条件苛刻得多。

**不为「更实用」而转向，而是原方向违反了自己写下的约束。**

---

## 2. 范围（边界 A）/ Scope — boundary A

**做：把各平台（免费 + 付费）的模型 API 收敛成一个统一网关。**

| 能力 | 说明 |
|---|---|
| 统一端点 | 对外一个 OpenAI 兼容入口，agent 与页面都只认这一个 |
| 密钥托管 | 各平台 API key 存 `secret(kind='model', name=...)`，明文（约束 13 取舍），永不出现在日志/回显/轨迹 |
| 路由 | 按 `model` + 价格 + 延迟 + 可用性选上游 |
| 故障转移 | 上游报错/超时时切下一个候选；**同一模型多上游才有意义** |
| 额度统计 | 按 key/模型/时间窗记录用量与花费 |

**上游先用现成 API**：OpenAI / Anthropic / Gemini / 各家聚合站 / 开源托管服务。

**不做（推后）**：本地开源模型推理（llama-server / vLLM 端点）。

## 2.1 为什么是边界 A 而不是 A+B

边界 A 与「网关 + 现在就支持本地端点」只差一块：provider 抽象。

`Provider` trait 让两种上游用同一套接口，因此**本地推理可以后加而不改架构**——
但**现在不写**。理由：目前没有任何用例能证明「哪些任务真的需要本地推理」，
现在就实现后端适配是在为一个未验证的需求付适配成本。等 Gateway 跑通、
看到真实的模型调用分布后再决定。

---

## 3. 与既有设计的关系 / Relationship to existing design

### 3.1 复用而非新建

| 已有 | Gateway 怎么用 |
|---|---|
| 设备密钥对 + 指纹注册（约束 5/11） | Gateway 的管理端沿用同一套登录/会话（`security-and-lifecycle.md` §2.4） |
| `secret` 表（`architecture/README.md` §3） | 模型 key 存 `secret(kind='model')`，已有 `kind` 枚举位 |
| **能力硬过滤 + 负载有序打分**（约束 9） | **同一套调度范式**。约束 9 的原则原样适用：能力用于排除，负载/价格用于排序，不做加权综合评分 |
| 任务路由 | **任务路由与模型路由是两套独立决策**，共享的不是节点能力而是「可选目标集合」——约束 9 的分法继续成立 |

### 3.2 约束 7 需要拆句（待 M1 规划时改 AGENTS.md）

当前约束 7 是两句，含糊：

> 7. **推理基于成熟开源项目**（llama.cpp / mesh-llm / prima.cpp 等），不自研推理内核，
>    也不自研无法审核的分布式推理代码。Rust 只做编排与网关。

后半句「Rust 只做编排与网关」正是 Gateway 的授权，但前半句只举了本地推理项目、
读起来像在限定推理实现方式。**M1 规划时应拆成两句**：

- **允许**：Hub 做网关编排上游（选路、转发、故障转移、额度统计）
- **禁止**：自研推理内核，或自研无法审核的分布式推理编排

> 本次**不改** `AGENTS.md`——M0 仍在进行中，改硬约束会干扰在途子任务。
> 到 M1 规划时连同 `inference.md` 重定位、`PRODUCT.md` §5.2 结论一起改。

---

## 4. 待决问题（规划时必须回答）/ Open questions

这些**故意没有答案**，因为现在回答会是拍脑袋。列在这里是为了规划时不遗漏：

1. **额度统计的粒度**：按 token 计费需要各平台的定价表，价格会变。是接平台返回的
   usage 字段自己算，还是定期同步定价表？定价表存哪、怎么更新？
2. **路由策略的用户控制**：约束 9 说过「可由用户钉死」。Gateway 下用户能配置什么粒度——
   每个模型钉一个上游？设权重？还是只能设「优先/备用」两组？
3. **流式响应的故障转移边界**：SSE 已经吐了部分 token 再切上游，会产生重复内容。
   是在首个 token 前允许切换，还是一旦开始流式就不切？
4. **健康检查的频率与判据**：多久探一次上游？判「挂了」是连接失败、5xx 还是超时？
   探活本身要不要计费（有些平台对探活请求也收费）。
5. **`Provider` trait 的最小接口**：要覆盖 chat / embeddings / 工具调用 / 流式 /
   多模态，第一版必须支持哪几个？`inference.md` 的本地推理后端要接进来时，
   最小接口能不能容纳它？
6. **本地推理后置的触发条件**：什么信号出现才值得实现本地端点？（例如：某个模型
   长期走付费且花费超预算，或离线场景必须可用）
7. **connector 侧的模型配置**：`PRODUCT.md` 提到「模型配置一处切换，对标 ccswitch」
   （§3 表）。Gateway 的模型配置与「Hub 统一配置 agent」的托管覆盖层是什么关系——
   两套配置源还是一套？

## 5. 明确不做的事 / Explicit non-goals

- 不做多租户配额、不做计费转售、不做 SaaS（与 `PRODUCT.md` §1.1 一致）
- 不做自研推理内核（约束 7）
- 不在 Gateway 里同步文件夹或走 P2P 直传（约束 10，与本功能无关）
- **不在 Gateway 层做任务编排**——任务路由仍按约束 8/9 在 Hub 层，与模型路由独立

---

## 6. 相关文档 / Related

- `docs/PRODUCT.md` §5.2——原始设计问题，本文是其结论
- `docs/architecture/inference.md`——本地推理可行性调研（存档，非主线）
- `docs/architecture/scheduling.md`——路由范式（能力过滤 + 有序打分）
- `docs/architecture/security-and-lifecycle.md` §2.4——登录与会话
- `AGENTS.md` 约束 7 / 9——需在 M1 规划时按 §3.2 拆句