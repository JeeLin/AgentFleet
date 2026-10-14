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
  "seq": 128,               // 连接内单调递增；重发沿用原 seq（去重靠它）
  "t": "hello",             // 消息类型
  "id": "m-128",            // 与 seq 对应；响应回填 request 的 id
  "node_id": "n_xxx",       // 握手后每帧必带
  "c": { /* 载荷 */ }       // content，按 t 决定 schema
}
```

- **`v` 协商**：握手时**双方各自声明 `v`，取较小者**作为本连接协议版本。
  只有破坏性变更（删了必需字段、改了语义）才 +1；新增可选消息类型 / 字段**不 bump 版本**。
- **未知 `t` 必须忽略而非断连**（前向兼容，见约束「版本≠功能」）。

---

## 3. 握手与认证（challenge / nonce）

**不直接凭公钥就放行**——用一次性 challenge 防重放：

```text
node → Hub:  hello { client_version, arch, os }        （尚无身份，node_id 为空）
Hub → node:  challenge { nonce }                          （Hub 生成的随机串）
node → Hub:  auth { node_id?, public_key?, signature }   （signature = sign(私钥, nonce)）
```

**首次 enroll**（`node_id` 为空）：

```text
node 本地生成密钥对 → 存 ~/.agentfleet/identity.json（0600）
node → Hub: auth { public_key, signature, device_info }
Hub 验签 → 查指纹是否已存在：
    存在 → 拒绝（防重复注册，见 security-and-lifecycle §4.3），除非携带正确 node_id
    不存在 → 签发 node_id，绑定指纹，登记
Hub → node: auth_ok { node_id, protocol_version }
```

**重连**（已有 `node_id`）：

```text
node → Hub: auth { node_id, signature(nonce) }
Hub 验签 + 比对该 node_id 的指纹 → 一致则 auth_ok
```

- **公钥只在首次 enroll 上传**；重连只传签名。
- nonce 一次性，用过即弃，防重放。

---

## 4. 消息类型（初版）

| 类型 | 方向 | 载荷要点 |
|------|------|---------|
| `hello` / `challenge` / `auth` / `auth_ok` / `auth_fail` | 双向 | 见 §3 |
| `heartbeat` | node→Hub | 能力+负载上报（字段见 `scheduling.md` §2），周期 15s |
| `heartbeat_ack` | Hub→node | 可选，快速确认 |
| `disconnect` | Hub→node | 令节点下线 `{ reason }`（`superseded` 被新连接顶替 / `admin` Hub 手动下线该节点），见 §7.1 |
| `open_channel` | Hub→node | `{ channel_id, connector, params }` |
| `channel_ready` | node→Hub | `{ channel_id, capabilities }` 或 `channel_error` |
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

> **不做**：自适应速率、优先级通道、跨通道公平队列——个人设备用不上，避免过度设计。

---

## 7. 重连与序列号

掉线后指数退避重连（1s/2s/4s…上限 30s，见 `security-and-lifecycle.md` §5）：

- **seq 在每条新连接内重新从 0 开始**，但 Hub 侧按 `(node_id, seq)` 记录最近 N 帧用于去重。
- **需要可靠投递的消息**（如 `open_channel` 的确认）带 `id`，重连后 Hub 可重发未确认的；纯推送帧（PTY 流）**不重传**——丢了就丢了，UI 标注「有 N 条已丢」。
- 重连成功后 Hub 下发「你有哪些活跃会话 + 工作区」，节点确认后重建通道（见 `task-continuity.md` §4.2）。

### 7.1 下线：被顶替 与 手动下线

两种场景共用一条 `disconnect` 消息，**区别只在节点收到后的重连策略**：

| `reason` | 触发 | 节点行为 | 为什么不同 |
|---|---|---|---|
| `superseded` | 同 `node_id` 的**新连接**通过验签后接管 | 旧连接退出，**停止重连**，进程休眠等下一次显式唤醒 | 若继续重连，新连接又把它挤掉 → 两边无限互抢 |
| `admin` | 用户在 Hub 点「下线此节点」 | 关闭 WS 后**按退避继续重连**（或进入受限态） | 设备还活着、用户只是不想它现在连着；不该因为手动下线就把它打死 |

```text
新连接 auth 通过 → Hub 发现该 node_id 已有连接
                → 内存表替换为新句柄
                → 向旧连接发 disconnect{reason:"superseded"}
                → 旧连接关闭 WS，标记「被顶替」，**不进入重连循环**
                → 新连接接管，此后所有通道归新连接
```

**关键**：Hub 侧替换是**先换表、后通知旧连接**（顺序不能反）——
若先关旧连接再换表，期间到达的消息会路由到一个已关闭的句柄上。

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