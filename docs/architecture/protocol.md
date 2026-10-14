# 节点协议 / Node Protocol

> 问题：node 与 Hub 之间的 WebSocket 协议长什么样？首次 enroll 怎么走？
> 掉线重连、通道复用、流控/信用、序列号重传各自怎么定？
> 这是 M0（设备接入）开工前必须钉死的协议层设计。

**约束前提**（来自 AGENTS.md）：每设备一条 WS，节点主动外联，通道键 `(node_id, channel_id)`，
密钥对本地生成，Hub 只存指纹。协议层的一切设计都服务于这几条。

---

## 1. 总体分层

```text
WS 连接（每设备一条，node → Hub 主动外联）
  └─ 帧层：JSON 帧 + 序列号（重传 / 去重）
      └─ 消息层：请求-响应 / 推送 / 事件
          └─ 通道层：channel_id 复用（terminal/files/git 等各自独立）
```

一条 WS 承载全部通道——**不因每通道一条 WS**（会撞防火墙限制，且与「每设备一条」冲突）。

---

## 2. 帧格式

JSON 文本帧（WS 层已有），信封统一：

```jsonc
{
  "v": 1,                    // 协议版本（不兼容变更才 +1）
  "seq": 128,                // 连接内单调递增；仅用于日志关联与调试，协议语义不依赖它
  "t": "hello",              // 消息类型
  "id": "c-7f3a",            // 客户端生成的请求 id，仅请求帧有；响应原样回填。推送帧不带
  "node_id": "n_xxx",        // 握手成功后必带；hello/auth/challenge 阶段可为空
  "c": { /* 载荷 */ }        // content，按 t 决定 schema
}
```

- **`v` 协商**：握手时**双方各自声明 `v`，取较小者**作为本连接协议版本。
  只有破坏性变更（删了必需字段、改了语义）才 +1；新增可选消息类型 / 字段**不 bump 版本**。
- **`seq` 不用于去重**：Hub **不做跨连接去重**（详见 §7）。跨连接的重传由 `id` + ACK 驱动。
- **未知 `t` 分档处理**（不是一律忽略）：
  - **必需集**（`hello/challenge/auth/auth_ok/auth_fail/heartbeat/err`）出现未知或缺失 →
    握手 10s 内未收到 `auth` 则关闭连接并 `err{code:"handshake_timeout"}`；
    已鉴权连接连续 45s 无心跳 → 判 offline（见 `scheduling.md` §2）。
    节点侧收到未知的**必需**消息类型 → 断开并退避重连（连接必然已不可用）。
  - **可选集**（`open_channel/channel_*/exec_*/disconnect`）**必须忽略**。
    Hub 对未知可选类型只记日志、不回 `err`；涉及用户操作的通道请求在
    `channel_ready` 超时 5s 后显式回 `channel_error{reason:"unsupported"}`——
    **不静默丢弃用户可见的失败**。
- **帧大小上限 1 MiB**：超出即断开并 `err{code:"frame_too_large"}`。

---

## 3. 握手与认证（challenge / nonce）

**不直接凭公钥就放行**——用一次性 challenge 防重放：

```text
node → Hub:  hello { client_version, arch, os, features }  （尚无身份，node_id 为空）
Hub → node:  challenge { nonce }                            （32 字节 CSPRNG，base64url；每连接一次）
node → Hub:  auth { node_id?, public_key?, signature }
```

**签名必须覆盖 `v + node_id + seq + t + nonce` 的规范编码**，而不只是 nonce：
否则一个被重放的 `auth` 签名可在任意新连接上复用。
`nonce` 用完即弃（连接关闭即失效）；连接建立后 **10s 未收到 `auth` 即关闭**。

**首次 enroll**（`node_id` 为空）：

```text
node 本地生成密钥对 → 存 <data_dir>/identity.json（0600，原子写，见 security §4.4）
node → Hub: auth { public_key, signature, device_info }
Hub 验签 → 【BEGIN IMMEDIATE】SELECT 指纹：
     已存在 → 拒绝 auth_fail{reason:"fingerprint already registered"}（除非带正确 node_id 走重连）
     不存在 → INSERT（key_fingerprint UNIQUE）→ 签发 node_id → 绑定指纹
Hub → node: auth_ok { node_id, protocol_version, gen, heartbeat_interval_ms }
```

- **enroll 必须是显式事务**（`BEGIN IMMEDIATE` → SELECT → INSERT → COMMIT）：
  顺序实现是 TOCTOU 竞态。唯一约束失败 = 并发 enroll → 回
  `auth_fail{reason:"fingerprint race, retry"}`，**绝不 upsert**——
  UPSERT 会让并发者覆盖别人的指纹绑定，导致已签发的 `node_id` 立即失效，
  而对方的私钥已落盘，永远接不回来。
- **node_id 用 128 位随机串生成**（避免 `n_xxx` 这种低熵可枚举）。
  `node_id` **不是秘密**（它出现在每个心跳帧里），**不得作为授权依据**。

**重连**（已有 `node_id`）——显式两步，不接受本次上传的任何密钥：

```text
node → Hub: auth { node_id, signature, last_seen_gen? }
Hub: ① 查该 node_id 绑定的 key_fingerprint（**忽略请求里带来的任何 public_key**）
     ② 用库内指纹对应的公钥验签
     ③ 失败 → auth_fail{reason:"identity mismatch"}，**绝不更新库内指纹**
     ④ last_seen_gen < 当前 connection_gen → auth_fail{reason:"superseded", gen}
        （此进程上一条连接是被顶替的，不做接管，见 §7.1）
```

- **公钥只在首次 enroll 上传**；重连只传签名。
- 这条挡的是「持有 node_id + 任意密钥对」的接管路径——最自然的实现顺序
  （先取本次公钥验签、再更新库内指纹）会让任何知道 node_id 的人把公钥绑上去。

---

## 3.1 初始化模式下的 Hub 未就绪

Hub 未初始化（`secret` 表无 `hub.password`）时，**拒绝所有 enroll**：
回 `auth_fail{reason:"hub not initialized"}`，提示先在 Web 上完成首次密码设置。
否则设备会在 Hub 设好密码前占住 `node_id`，且无法覆盖。

**`/ws` 在初始化模式与正常模式下均豁免 session 认证**（node 不是浏览器、没有 cookie）；
浏览器侧的其余 API 才需要 session。完整路由豁免表见 `security-and-lifecycle.md` §2.4.4。

---


## 4. 消息类型（初版）

| 类型 | 方向 | 载荷要点 |
|------|------|---------|
| `hello` / `challenge` / `auth` | 双向 | 见 §3（`hello` 带 `client_version`/`arch`/`os`/`features`） |
| `auth_ok` | Hub→node | `{ node_id, protocol_version, gen, heartbeat_interval_ms }`——`gen` 见 §7.1（顶替兜底），`heartbeat_interval_ms` 是本连接的心跳周期 |
| `auth_fail` | Hub→node | `{ reason, gen? }`——`reason` 含 `hub not initialized`/`fingerprint already registered`/`fingerprint race, retry`/`identity mismatch`/`superseded`（见 §3、§3.1） |
| `hub_hello` | Hub→node | `{ features, heartbeat_interval_ms, protocol_version }`——Hub 侧能力声明（约束 14 的对称性：协商是双向的，不只节点声明） |
| `heartbeat` | node→Hub | 能力+负载上报（字段见 `scheduling.md` §2），**周期由 `auth_ok` 的 `heartbeat_interval_ms` 下发**（默认 15000，节点不硬编码） |
| `heartbeat_ack` | Hub→node | 可选，快速确认 |
| `disconnect` | Hub→node | 令节点下线 `{ reason }`（`superseded` 被新连接顶替 / `admin` Hub 手动下线该节点），见 §7.1 |
| `open_channel` | Hub→node | `{ channel_id, connector, params }` |
| `channel_ready` | node→Hub | `{ channel_id, capabilities }` |
| `channel_error` | 双向 | `{ channel_id, code, reason }`；`channel_ready` 超时 5s 由 Hub 显式回 `{reason:"unsupported"}` |
| `channel_data` | 双向 | 通道内数据（PTY 字节、文件读写结果等） |
| `channel_credit` | Hub→node | 信用补充 `{ channel_id, delta }`（**独立于 `channel_data`**，见 §6） |
| `close_channel` | 双向 | `{ channel_id, reason }` |
| `exec_request` / `exec_result` | Hub→node→Hub | 一次性命令（无 PTY） |
| `err` | 双向 | 通用错误 `{ code, message }` |

> 通道类型（terminal/files/git）各自的 payload schema 随 M1 定义；本里程碑先立框架。

---

## 5. 通道（channel）复用

- **通道键 `(node_id, channel_id)`**——`channel_id` 只在**该设备内**唯一，不是全局 id（约束 4）。
- 每通道独立生命周期：开 / 用 / 关。关通道不影响其他通道与 WS。
- 一个 WS 上同时承载多通道；**每通道独立流控**（§6），一个跑飞的传输不拖死其他通道。
- 通道请求-响应用 `id` 配对；推送型数据（PTY 流）不需 id。

---

## 6. 流控与信用（credit）

约束 4 承诺「每通道独立流控/信用」，此处定最小可用模型：

```text
Hub → node: open_channel 时带 credit_start（如 32）
node 收到 credit_start → 维护该通道的剩余 credit
node 每消费 credit 发 channel_data → credit -= 1；credit 归零则暂停发
Hub 每收到 N 帧或定时 → 补 credit（如 +32），信用只增不减
```

- **信用按通道独立**，不跨通道共享。
- 目的是防「一个 PTY 输出洪峰打爆 WebSocket / 内存」，不是限速到网络带宽。
- PTY 这类单向流：节点侧靠 credit 节流向 Hub 推；Hub→节点的写入（用户输入）量小，靠 WS 自身背压即可。
- **每次 `open_channel` 重置该通道 credit 为 `credit_start`**；重连后 Hub 必须重新
  `open_channel` 才继续发数据——否则「信用只增不减」会让 Hub 侧按 `channel_id` 无限累积。

### 6.1 credit 只管发送方自愿节流，Hub 侧必须有硬上限

credit 是**发送方的自我约束**，不是 Hub 的自我保护。一个 buggy 或恶意的 worker
完全可以在 credit 耗尽后继续发 `channel_data`——而协议层没有别的机制拦它。
因此 Hub 侧必须有两条不依赖对方配合的硬限制：

```text
读侧（node → Hub）：每连接一个 semaphore，默认 64 帧
   超限 → 该连接的 reader 慢读/暂停
   持续超阈值（如 30s）→ 主动断开并 err{code:"flood"}
写侧（Hub → node）：每连接有界写队列，默认 256 帧
   满 → 控制面消息（disconnect / close_channel）**优先不排队，直接发**
        数据帧丢弃并回 err
```

**没有这两条，credit 机制是单方面的**：一台设备持续灌数据 → axum/hyper 逐帧解析
JSON 写进无界 `Vec` → **Hub OOM，一台设备打垮整个 Hub（连带所有其他设备与 Web 端）**。
而这正是 credit 想防的事，却只防了 node 的发送侧。

> **不做**：自适应速率、优先级通道、跨通道公平队列——个人设备用不上，避免过度设计。

---

## 7. 重连与序列号

掉线后指数退避重连（1s/2s/4s…上限 30s，见 `security-and-lifecycle.md` §5）：

- **seq 在每条新连接内重新从 0 开始**；Hub **不做跨连接去重**。
  （曾设计过「按 `(node_id, seq)` 记录最近 N 帧去重」，但它会误杀合法首帧：
  设备重连后第一条消息必为 `seq=0`，命中上次连接的记录被丢弃 → 能力缓存永不更新，
  而丢的是纯推送帧、永远不会自愈。且「最近 N 帧」的 N 与超时未定义，
  而跨连接的重传本就由 `id` + ACK 驱动，没有任何东西需要它。）
- **需要可靠投递的消息**（如 `open_channel` 的确认）带 `id`，重连后 Hub 可重发未确认的；纯推送帧（PTY 流）**不重传**——丢了就丢了，UI 标注「有 N 条已丢」。
- 重连成功后 Hub 下发「你有哪些活跃会话 + 工作区」，节点确认后重建通道（见 `task-continuity.md` §4.2）。
- **节点在收到 `auth_ok` 后立即发一次心跳**，之后按 `heartbeat_interval_ms` 周期发
  （Hub 用下发的间隔，不硬编码 45s——这是约束 14「版本 ≠ 功能」的又一例）。

### 7.0 退避必须带抖动

退避是**带抖动的确定性序列**，不是纯指数：

```text
sleep = min(30s, 2^attempt) * uniform(0.7, 1.3)
每次成功连接后 attempt 归零
```

**为什么必须有抖动**：同机两个进程（或重启后残留的旧进程）同时重连时，
若退避都从 1s 起且无抖动，两次尝试会**几乎同时**打到 Hub → A 顶掉 B、B 顶掉 A →
各自再等 1s → **完全同相**。两条连接将永远交替胜出，稳定活锁。
`uniform(0.7, 1.3)` 打破同相，让其中一方胜出后另一方已退出。

### 7.1 下线：被顶替 与 手动下线

两种场景共用一条 `disconnect` 消息，**区别只在节点收到后的重连策略**：

| `reason` | 触发 | 节点行为 | 为什么不同 |
|---|---|---|---|
| `superseded` | 同 `node_id` 的**新连接**通过验签后接管 | 旧连接退出，**持久化标记 `superseded_at`**，睡眠等显式唤醒 | 若继续重连，新连接又把它挤掉 → 两边无限互抢 |
| `admin` | 用户在 Hub 点「下线此节点」 | 关闭 WS 后**按退避继续重连**（或进入受限态） | 设备还活着、用户只是不想它现在连着；不该因为手动下线就把它打死 |

```text
新连接 auth 通过 → Hub 发现该 node_id 已有连接
                → connection_gen += 1（单调递增，写入 registry）
                → 内存表替换为新句柄          ← 先换表
                → 向旧连接发 disconnect{reason:"superseded", gen}
                → 旧连接关闭 WS，持久化「被顶替」标记，**不进入重连循环**
                → 新连接接管，此后所有通道归新连接
```

**关键一：Hub 侧替换是「先换表、后通知旧连接」**（顺序不能反）——
若先关旧连接再换表，期间到达的消息会路由到一个已关闭的句柄上。

**关键二：`disconnect` 帧可能丢失，必须有兜底路径。**
`disconnect` 走普通 WS 发送路径，而 §7 已声明推送帧会丢。若这一帧正好丢了：
旧连接 30s 后重连 → 验签通过 → Hub 再换表 → 再丢 → **每 60s 顶替一次，永久进行**。
同机两个 supervisor 时表现为「设备反复上下线」。

因此**「停止重连」不能只依赖消息送达**：

```text
兜底路径（不依赖任何消息投递）：
  旧连接持久化 superseded_at 与 Hub 返回的 gen（含 supervisor 重启后仍生效）
  下一次重连时 auth 带 last_seen_gen
  Hub 见 last_seen_gen < connection_gen
    → 回 auth_fail{reason:"superseded", gen}，**不做接管**
    → 节点据此确认为被顶替，停止重连
```

`gen` 让「我是否已被顶替」成为一个**可由 Hub 单方面判定的状态**，
而不是赌一条可能丢失的消息。两条路径（帧送达 / `auth_fail` 兜底）必须都测。

> **手动下线的取舍**：`admin` 下线让节点继续重连，是为了「设备没坏、稍后自动恢复」。
> 若用户想**彻底停掉这台设备**，那是设备运维问题（停服务 / 断电），不靠 Hub 下线实现——
> Hub 不持有能永久封禁一台设备的权力（约束 1：Hub 不做设备管控，只做编排与观察）。

---

## 8. 兼容与能力协商

- **功能由能力协商决定，不靠版本号猜**（约束 14：版本 ≠ 功能）：
  - `hello` / 心跳里带节点支持的 feature 列表，Hub 按能力下发，不按 `v` 猜测。
- 协议保持向后兼容：新增消息类型可选，**老节点忽略未知 `t`**。
- 不兼容（节点太旧、必需的 feature 缺失）→ **明确 `auth_fail` 并提示升级，不静默失败**。

---

## 9. 与 M0 验收的关系

M0 枚举的「防第二台抢走第一台身份」= §3 的两处保证：

1. **重连认身份**：`(node_id, 签名(nonce))`，验签+比对指纹，不是「谁先连上算谁的」。
2. **enroll 防重复**：指纹已存在则拒绝新 `node_id`（§3、§4.3 security-and-lifecycle）。

「防第二台抢走第一台身份」**不是拒绝第二个连接，而是顶替**：
同一 `node_id` 的新连接通过验签后，Hub 关闭旧连接并用新连接接管（§7.1）。

---

## 10. 留待 M1 细化

- terminal / files / git 各通道的 `channel_data` 具体 schema
- PTY 流的分片与帧大小上限（留给 M1 的 exec/terminal 实现）
- 文件传输的大文件走 CAS blob 还是通道内分片（M2 再定）