# 安全与生命周期 / Security & Lifecycle

> 覆盖三个此前没有设计的问题：**凭据怎么存**、**节点怎么升级**、
> **设备掉线与恢复**。

**核心决策（本项目已定）：**

1. 设备密钥对**在 node 本地生成**，Hub **只存指纹**。
2. connector 凭据与模型 API key **由 Hub 明文存储，不做应用层加密**——
   靠文件系统权限收紧，**且永不打日志、永不回显**。
3. node 分 **supervisor / worker** 两层（同二进制，角色由 `run --worker` 区分），
   **升级只换 worker，身份不变**——执行回滚的 supervisor 不参与被回滚。

---

## 1. 凭据：分三类，别混

| 类 | 谁生成 | Hub 存什么 | 传输方向 |
|---|---|---|---|
| **A. 设备密钥对** | **node 本地生成** | **只存指纹** | node → Hub |
| **B. Hub 登录凭据** | 用户设置 | 密码哈希（不可逆） | 浏览器 → Hub |
| **C. connector 凭据 / 模型 API key** | 用户提供 | **明文**（本项目决定） | Hub → node |

**A 类没有选择余地**：若 Hub 生成设备私钥，Hub 就等于持有全部设备的私钥，
密钥对的意义归零。**必须是 node 首次运行时本地生成，私钥永不上传。**

---

## 2. 明文存储：取舍与必须配套的防护

**决定：不做应用层加密。** 这是单用户自托管场景下的合理取舍，
但它的真实含义必须写清楚：

- 「不加密」不是「安全」，是**把防护责任挪到文件系统权限上**。
- 暴露面 = **Hub 的 SQLite 文件 + 任何备份 + 任何误提交 + 任何一次 XSS**。
- 单用户自托管确实降低了「攻击者拿到数据库」的概率，
  但**不改变 Hub 被攻破即横向进入全部设备**这一后果。

> 明文存储最大的实际风险不是磁盘泄露，而是**不小心打进日志或 API 响应**。
> 因此下面三条是**强制项**，不是建议。

### 2.1 强制防护（三条，缺一不可）

```text
1. 权限：SQLite 文件 0600，数据目录 0700，Hub 进程 umask 077
2. 出口：secret 表的值永不进日志、永不进 API 响应、永不进轨迹事件
3. 回显：前端只拿到 { configured: true, fingerprint: "sk-a***xyz" }
```

**第 3 条的 fingerprint 规则**：`前 3 字符 + "***" + 后 3 字符`。
用户需要靠它确认「配的是哪个 key」，但不能靠它还原。

### 2.2 为什么加解密并不会让存储/迁移复杂多少（先量级，再决定）

加密的真实成本**不在存储**，在**密钥管理**：

| 加的东西 | 成本 | 能否事后加 |
|---|---|---|
| `secret.value` 存 ciphertext 而非明文 | 一列改类型 + 一个加解密函数 | ✅ 可，且可双读迁移 |
| 主密钥文件放哪 | **这才是真成本** | — |
| 多一条「密钥从哪来」的来源问题 | 部署时的第一次配置 | — |

存储层没有额外成本（`TEXT` 变 `BLOB` 而已）；**迁移也不需要额外成本**
——密文原样搬，只搬主密钥。真正的成本只有一个：**你必须决定主密钥从哪来。**

而这正是「明文」最有说服力的地方：**明文 = 零决策成本**，
这符合自托管工具的哲学——装起来就能用，不逼你先做一次密钥管理体操。

> 本项目的决定：**先明文，零配置成本**；把主密钥来源这个问题留到真的需要时再回答。

### 2.3 记为 TODO 的加固项

以下**本期不做**，但要在 `BUGS.md` 留条目，未来需要时启用
（届时唯一的前置决策是「主密钥从哪来」）：

- `secret` 表加 AES-256-GCM 静态加密，主密钥独立存放
- secret 值进内存后不可换出（mlock / 不在 core dump 里）
- 密钥轮换与失效

**迁移路径已预留**：`secret` 表从第一天起就带 `kind` + `name` 两列作为主键，
不依赖值本身；因此未来加密时可双读（`value` 有密文则解密，无则当明文），
不需要停机改表结构。

---

## 2.4 Hub 登录（单用户，首次访问设密码）

单用户不等于无认证——Hub 监听在 Web 端口上，任何能访问该端口的人都能操作
你的全部设备（会话、文件、exec）。**唯一的用户也要证明自己是谁**，
否则一个局域网里的陌生人就能读你的代码、跑你的命令。

### 2.4.1 首次访问设密码（不是内置密码，不是无密码）

```text
Hub 首次启动  → secret 表无 hub.password → 进入「初始化模式」
                生成 16 字节 setup_token，只写 stderr 与 <data>/setup-token.txt（0600）
                只服务 setup：页面 /setup（SPA 外壳）与端点 POST /setup
                设置完成 → 写 password 后进入正常模式，setup_token 失效并删除
此后每次启动  → 已有密码 → 正常模式，GET /api/setup 返回 409
```

- **没有默认密码**，不存在「忘了默认密码」这回事。
- **`POST /setup` 必须带 `X-Setup-Token`**，否则 403。
  **没有 token，任何人可抢先初始化你的 Hub**：Hub 常起在 `0.0.0.0:8080`
  （树莓派 + 端口转发 / Tailscale 同网段 / Docker bridge 邻居），
  用户还没打开浏览器，同网段另一台机器
  `curl -X POST /setup -d '{"username":"me","password":"me"}'` 就拿到了 admin。
  token 只落 stderr 与数据目录文件，攻击者拿不到。
- **setup_token 失败 10 次即失效**，需**重启 Hub** 重出（防爆破）：
  启动时未初始化就生成新 token 并写回 `<data>/setup-token.txt`，失败计数随之清零，
  所以「重启 Hub」已经是完整且准确的恢复路径（不另设恢复命令）。
- **`/setup` 与 `/api/setup` 是一件事的两半**：`/setup` 是页面与初始化端点
  （`POST`），`/api/setup` 是初始化状态探针（`GET`）。一条路径不能既回 HTML
  又回 JSON，硬加载 `/setup` 必须拿到页面。
- **初始化模式除 `/setup` 与 `/api/setup` 外一律 403**——设置密码前不能读设备、不能开会话。
- 请求体里的 `username` 字段**收下但不落库**：M0 只有一个操作者，
  `username` 无处可存（多用户是 M1 的范围选择，见 §2.4.4）。
- **密码哈希用 Argon2id**（不是 SHA-256：快哈希能被 GPU 暴力破解）。
  存 `argon2id$m=65536,t=3,p=4$<salt>$<hash>`，**salt 每用户随机 16 字节**。
  这是**唯一**不可逆存储的凭据（A/C 两类见 §1，A 只存指纹、C 明文）。
- **Argon2id 校验必须走 `tokio::task::spawn_blocking`**，禁止在 tokio worker 上同步阻塞：
  `m=65536,t=3,p=4` 单次 50–200ms，在 4 核树莓派上会阻塞 worker，
  叠加每 15s × N 设备的心跳落库与 `/ws` 读循环，一次登录尝试就能让心跳延迟到超时边缘。
  而 `/login` 是未认证端点，攻击者反复触发即可放大。
  参数抽成常量并允许按 `nproc` 下调；`/login` 加全局信号量（=2）+ 排队超时 429。

### 2.4.2 会话：cookie，不是 token

- 登录成功签发 **httpOnly + SameSite=Strict** 的 session cookie，
  **不放在 localStorage**（防 XSS 读取）。
- **session 的 HMAC 密钥 = Hub 首次初始化时生成的 32 字节随机值**，
  存 `secret(kind='hub', name='session_hmac')`（**随数据库一起备份**；
  「0600」指数据库文件本身的 owner-only 权限——`secret` 表没有按行的
  权限机制）。
  - **不用明文密码当签名密钥**：改一次密码会使全部会话失效。
  - **不用设备指纹派生**：这是**首次部署死锁**——`device` 表为空（未注册任何设备）时
    该密钥无法计算，Hub 在 `POST /setup` 之前就签发不出 session；
    而指纹又是公开值（`node_id` 由它唯一决定，见 §4.0），
    **公开值当签名密钥 = 无认证**，任意知道 node_id 的人可离线伪造任意用户的 session。
- **M0 用纯签名 cookie，不建服务端 `session` 表**（表清单见 `architecture/README.md` §3）。
  签名 cookie 天生无法被服务端撤销——这是已知取舍：改密码**不会**让已签发的 cookie 失效
  （只有轮换 `session_hmac` 才会）。单用户自托管场景下可接受（能读到 cookie 的人
  已经能访问这台机器了）。若要「强制注销 / 踢下线 / 查看活跃会话」，随 M1 加表。
- **CSRF**：所有状态变更走 `POST/PUT/DELETE` + `SameSite=Strict` + 自定义 header 双重提交。
- 登录失败有速率限制（IP + 尝试次数），防局域网内暴力破解。

**CSRF 只对「已带 session 的状态变更」强制**（实现见 `hub-server/src/middleware_auth.rs`）：
`/setup` 与 `/login` 是 pre-auth 端点，请求时还没有可被「骑乘」的登录态，
所以不要求 `X-CSRF-Token`——防它们靠的是 setup token 与密码本身，
`SameSite=Strict` 拦不住的是「同源 XSS 发起的请求」，而这靠 HttpOnly
（`/setup` 与 `/login` 都不下发可写的 session cookie）兜底。
session 一旦签发，**所有**状态变更都要双重提交。

### 2.4.3 路由豁免表（node 不是浏览器）

`/ws` **在初始化模式与正常模式下均豁免 session 认证**——
node 不是浏览器、没有 session cookie，它走 §3 的 challenge/签名认证。
没有这张表，「除 `/setup` 与 `/api/setup` 外全部要求 session」会把**所有设备挡在门外**。

| 路由 | 初始化模式 | 正常模式（无 cookie） | 正常模式（有 cookie） |
|---|---|---|---|
| `/setup`（页面） | 豁免（SPA 外壳） | 豁免 | 豁免 |
| `/api/setup`（探针） | 200 `{initialized:false}` | 409 | 409 |
| `POST /setup`（端点） | 需 `X-Setup-Token` | 409 | 409 |
| `/login` | 403 | 可用 | — |
| `/ws` | **豁免**（走 §3 认证） | **豁免** | **豁免** |
| `/healthz` | 豁免（无敏感信息） | 豁免 | 豁免 |
| 其余 API | 403 | 401 | 需 cookie |

### 2.4.4 与单用户范围的关系

- **不做**：多用户、RBAC、团队协作、注册邀请（约束 8：范围选择，不引入）。
- **不做**：OAuth / 第三方登录（自托管工具多一个外部依赖就多一个失效点）。
- 「单用户」的含义 = **一个 Hub 只有一个操作者**，但**仍要认证**。
  把「单用户」读成「不用登录」是危险的误读——Hub 暴露在 Web 端口上。

---

## 3. 模型 API key：下发到所有设备 vs 每台自带

| 方案 | 一处改处处生效 | key 是否出 Hub | 代价 |
|---|---|---|---|
| **A. Hub 统一下发到所有节点** | ✅ 是 | **是** | 一把 key 泄露 = 全部设备 + 烧钱 + 冒用对话 |
| **B. 每台设备自带 key** | ❌ 否（N 次配置） | 否 | 配置麻烦，但泄露面只有一台 |

**第一版取 A**（对应「Hub 一处配置模型与 API key，各节点复用同一套配置」，
对标 ccswitch 的交互：先在 Hub 配好一处，各节点上的 agent 立即复用）。

**代价已知并接受**：A 方案下模型 key 是**最高价值的单点秘密**。

### 3.1 一处配置如何传播

用户在 Hub 改一次 → Hub 更新 `secret` 表 + 重算托管清单里相关文件的哈希
→ 下一次心跳，节点报「我这份配置漂移了」→ Hub push 新版本 → 节点落盘。

**不是实时广播**：等心跳周期（秒级）自然收敛，无需额外的推送通道。

---

## 4. 设备生命周期：supervisor / worker

### 4.0 身份 = `node_id` + 密钥对，二者绑定

**`node_id` 不是秘密、不得作为授权依据**：它是 128 位随机串，
但明文出现在每个心跳帧里（Hub 日志、前端都可能留下）。
授权只看「能否用库内指纹对应的私钥签出合法签名」——
**重连时绝不接受请求里带来的任何公钥**（详见 `protocol.md` §3）。

**设备身份的唯一性由密钥对保证，`node_id` 只是 Hub 侧的索引。**

- 密钥对在 node 本地生成 → 上传公钥 + 指纹 → Hub 签发 `node_id` 并绑定该指纹。
- 之后每次 WS 连接都用私钥签名，Hub 验签 + 比对指纹 → 确认「你还是你」。
- **唯一性锚点是密钥对**：`node_id` 唯一、指纹唯一、且二者一一绑定。
- 因此「防止两台设备互相顶号」= **同一指纹不允许绑定两个 `node_id`**。

### 4.1 两层分工

```text
supervisor（极小，不碰业务；文件被替换但进程继续跑旧映像）
  ├── 拉起 / 监控 worker（看退出码）
  ├── 替换二进制 / 回滚          ← 关键：执行回滚的代码不参与被回滚
  └── 写 update-state.json
        │
        └── worker（afcli run --worker，同一二进制）
              ├── 持有设备密钥对        ← 身份的不变量
              ├── 维持与 Hub 的 WebSocket
              └── 实际执行 connector 与 agent；升级时换二进制 + 重启
```

**为什么密钥在 worker 而不在 supervisor**：两层都要存在，但**职责完全不同**——
supervisor 唯一要做的是「保证有一个能用的 worker」，它不需要理解业务，
因此没有 bug 面；而这恰恰是最需要永远可靠的那个进程。
让 supervisor 也持有密钥与 WS，等于让它重新变成一个可能出错的业务进程。

**回滚为什么可靠**：supervisor 替换二进制时，**它自己没有被替换**——
文件换新了，进程继续跑旧映像（下次被 systemd/docker 拉起才更新新代码），
所以新版本再坏，也坏不到执行回滚的那份代码里。

> 「下次被 systemd 拉起」这句在 M2 之前只是一句假设——仓库里没有 unit，
> 用户得自己从零写一个。现在有 `deploy/afcli.service`（`Type=simple`、
> `Restart=always`、配置走 `EnvironmentFile`），本节与 `README.md` 的生产段
> 都指向它。**两层分工不可合并**：systemd 管进程存活（supervisor 自己没了），
> supervisor 管 worker 生命周期（worker 崩了怎么退避、何时判坏版本）。

> supervisor 的职责被显式约束为一个**极小的稳定 ABI**：`spawn` + 退出码协议 +
> `update-state.json`。它必须向后兼容——这是上面「懒生效」能成立的前提。

#### 4.1.1 落地情况（M0 子任务 4）

两条承诺**只在真进程上验证过**（`crates/node-cli/tests/supervisor_lifecycle.rs`：
真 Hub + 真 `afcli`，`kill -9` 掉 worker 再观察）：

- **换 worker 不换身份**：kill 掉 worker 后，Hub 侧重新出现的连接带的是同一个
  `node_id`，且全程不出现第二个 `node_id`。身份在 worker 的密钥对里、由 Hub 按指纹认，
  所以进程换了设备没换。若身份挂在 supervisor 上或每次 spawn 都重新 enroll，
  这条测试会看到两个不同的 `node_id`。
- **换 worker 期间 Hub 不判离线**：采样 20s 内 `status` 一次都没变成 `offline`。
  之所以成立有两个前提：supervisor 自己从不持连接（「换 worker」对 Hub 只是
  一次普通重连），以及**离线判据只看心跳年龄**（§5，45s）——2–5s 的重连窗口
  远不到阈值。若断开即清内存上报，这里会立刻 offline。

`kill -9` 是被信号杀死，`ExitStatus::code()` 是 `None`，supervisor 必须把它当成
`CRASH`(12) 而不是让 `-9` 落进 i32 里——后者会让 supervisor 自己 panic。

### 4.2 为什么必须两层

若只有一个进程：升级 = 重启 = **WS 断开 + 身份需重新协商**。
有了 worker：升级时 **WS 短暂断开 → worker 用同一密钥重连 → 同一个 `node_id`**。
**不需要任何身份抢占逻辑。**

⚠️ 换 worker 期间（约 2–5s）Hub 会短暂看到设备重连，但**不是** supervisor 断开——
supervisor 根本不持有连接，所以它不会因升级而抖动。

### 4.3 重复注册的防护（M0 验收项）

**注意：这与升级无关，是另一件事。** 防的是「同一密钥被重复 enroll」
（密钥被克隆 / 用户手滑跑两次 `enroll`）：

| 场景 | 处理 |
|---|---|
| 同一 `node_id` + 同一指纹，正常重连 | ✅ 正常，按 §5 重连处理 |
| 同一 `node_id` + 同一指纹，**第二个 WS 要抢占** | ✅ **不拒绝——新的直接顶掉旧的**：Hub 用新连接接管，向旧连接发 `disconnect{reason:"superseded", gen}`，旧连接退出且**不再重连**。**但「不再重连」不能只依赖帧送达**——帧丢失时靠 `auth` 带 `last_seen_gen` 让 Hub 回 `auth_fail{superseded}` 兜底（见 `protocol.md` §7.1） |
| 新 `node_id` + **已存在的指纹** | ❌ 拒绝，`auth_fail{reason:"fingerprint already registered"}`——这才是「另一台机器」 |
| **并发 enroll**（两台同时首次注册） | ❌ 走显式事务 + `UNIQUE(key_fingerprint)`；唯一约束失败回 `auth_fail{reason:"fingerprint race, retry"}`，**绝不 upsert**（见 `protocol.md` §3） |
| **正确 `node_id` + 错误公钥 + 合法签名** | ❌ `auth_fail{reason:"identity mismatch"}`，**绝不更新库内指纹**——否则「持有 node_id + 任意密钥对 = 完整控制该设备」 |
| 同名设备重复 enroll | 提示已有记录，问用户是复用还是新建设备 |

#### 4.3.1 落地情况（M0 子任务 7）

兜底判据是 **`last_seen_gen` < 当前**活着**那条连接的 `gen`**，不是「< Hub 累计发出
过的 gen」。后者看起来更保守，实际是错的：`gen` 每条连接握手前新分配一次，
于是**每一次正常重连**都满足「带去的 < 本次拿到的」，设备会被自己的兜底机制
永久锁在门外且无自愈路径。改成跟活连接比之后语义才对得上：

- 顶替者**还活着** → 旧连接带着落后 gen 回来 → `auth_fail{superseded, gen}`，不接管；
- 顶替者**自己也走了** → 没有第二条连接跟它抢，旧连接回来就无害，**必须放行**。

另外两点实现约束：

- `last_seen_gen` 缺省（`None`）视为「没见过 gen」**而不是 0**。老节点的帧里没有
  这个字段，按 0 处理会让它们永远被判成已被顶替（约束 14 前向兼容不许这样断）。
- 「被顶替」要**持久化**到身份文件，不能只在内存里。supervisor 重拉出的新进程
  什么都不记得，只看内存会老老实实再连一次。清这个标记需要人工介入——
  「被顶替」意味着同 `node_id` 另有连接活着，自动重试就是互抢。

**为什么「顶替」而不是「拒绝」**：同一台设备重连（换网络 / 网络抖动恢复 / supervisor 重启）
会带来「新旧连接短暂并存」。若一律拒绝，重启中的设备会被自己的旧连接挡在门外；
若新旧都放行，两边会无限互抢。**唯一性锚点是密钥对（§4.0），不是 `node_id` 字符串**——
`node_id` 由指纹唯一决定，所以「同指纹 + 合法签名」= 同一台设备，不存在「第二台抢第一台」。

> 真正要拒绝的只有一种：**拿不到合法签名**（密钥被克隆 / 伪造）。
> 那才是安全事件，走 `auth_fail`，不是「顶替」。

---

### 4.4 identity.json：格式、权限、原子写（M0 验收项）

私钥文件是**整个安全模型的根**——它决定了「你是不是你」。
格式与写入方式必须钉死，否则会出现「节点永久无法重新接入」的故障。

```text
<data_dir>/identity.json   （0600）
{
  "version": 1,
  "node_id": "n_<128-bit random>",          // 首次 enroll 后由 Hub 签发回填
  "public_key": "<Ed25519 public, base64>",
  "private_key_pkcs8_b64": "<Ed25519 PKCS#8, base64>",
  "created_at": "<RFC3339>"
}
```

- **算法：Ed25519**（不是 RSA/ECDSA——密钥短、签名快、实现简单，适合树莓派）。
- **写入必须原子**：`临时文件 → fsync → rename`。**绝不用 `File::create` 直接写**：
  进程在写到一半时崩溃 → 身份文件损坏 → 节点重启生成**新密钥对** →
  重新 enroll 撞上 §4.3「指纹已存在则拒绝」→ **设备永久无法接入，只能手工删库**。
  这是最可能实际发生的故障。
- **权限在创建时就用 `OpenOptionsExt::mode(0o600)` 落**，**不依赖 umask**：
  systemd 单元的 umask 常是 0022，靠 umask 会得到 0644，
  写入期间私钥以世界可读存在（同机任意用户 / 容器挂载可读）。
- **读取时若权限宽于 0600，先收紧并告警**（不要静默继续用）。
- **私钥永不出现在**：任何 WS 载荷、日志、错误信息、trajectory 事件
  （约束 13）。对 `Debug` 实现加断言，防止 `#[derive(Debug)]` 把它打进日志。
- 文件名统一为 `identity.json`（`PRODUCT.md` / `workspace-and-sync.md` 中的旧称 `auth.json` 已同步修正）。
- **`<data_dir>` 按优先级解析**：`AFCLI_DATA_DIR` 优先，否则平台标准目录
  （Linux `~/.local/share/agentfleet`、macOS `~/Library/Application Support/agentfleet`）；目录 0700。

---

## 5. 掉线与重连

```text
WS 断开 → 节点指数退避重连：1s / 2s / 4s … 上限 30s（带抖动，见 protocol §7.0）
        → Hub 侧 node 标记「重连中」
连续心跳丢失 > 45s（= 3 个心跳周期）→ device.status = 'offline'
```

> **为什么是 45s 而不是 3 分钟**：**判定只看心跳，不看重连次数。**
> 「连续失败 3 分钟」是另一套时钟——设备可能**心跳超时但重连成功**，
> 此时连接是通的，`UPDATE status='offline'` 会把活着的设备标成离线，
> 而调度硬过滤第一步就是「online？否→排除」，活着的设备被排除了。
> 45s = 3 个周期，足以过滤绝大多数网络抖动 / 短暂休眠 / 一次 GC 停顿。
>
> **这是启发式阈值，不是协议保证**，已知取舍：**Hub 自身重启会误判所有设备离线**。
> M0 接受（重启后设备会在下个心跳周期内自行恢复）。
> 若要消除，实现 Hub 侧持久化的 `grace_until`（与 DB 同事务写），不得改协议层。
>
> **「重连中」只存在于内存，不落库**：`device.status` 只有 `online` / `offline`，
> 但内存里的连接状态有第三种 `reconnecting`。调度硬过滤把 `reconnecting`
> **视为可用**——PTY 活着、工作区活着、会话活着，与 `PRODUCT.md` §3.3 流程 7
> 「离线期间工作区与 agent 会话在设备上仍存活」一致。

### 5.1 离线期间发生了什么

- **设备上**：工作区、PTY、agent 进程**都还活着**（只是没有 Hub 可达）。
- **Web 端**：面板转**只读**，显示最后已知状态；**禁止发起新操作**。
- **用户可**：手动点「立即重连」，跳过退避。

### 5.2 重连后

见 `task-continuity.md` §4.2：按 `(node_id, path)` 重建 terminal/files/git 通道，
PTY 缓冲区与 git 状态在重连后**不复位**（它们在节点进程里活着）。

### 5.3 落地情况（M0 子任务 6）

- **扫瞄器**：`hub-server/src/presence.rs`，周期 15s（`SCAN_INTERVAL`），
  每轮把 `timed_out` 列出的设备 `mark_offline`。判定只查心跳年龄。
- **`Reconnecting` 靠「断开时不删上报」生效**：内存上报活过重连窗口，
  `presence()` 才有「WS 断了但心跳新鲜 → 调度仍算可用」这一档。
  断开即删会让这档永远走不到，`PRODUCT.md` §3.3「离线期间会话仍存活」
  的承诺随之落空。
- **回 online 不靠扫瞄器**：靠下一次心跳的 `touch_online`。扫瞄器只会
  往 offline 改，所以恢复后不需要任何额外动作。

> ⚠️ **规格内部冲突（待裁决）**：里程碑 M0 子任务 6 的测试标准④ 要求
> 「心跳超时但 WS 仍通 → `device.status` 仍为 `online`」，与本文档 §5、
> `protocol.md`「连续 45s 无心跳 → 判 offline」、`scheduling.md` §2 三处
> 一致的表述相反。实现按文档侧（标 offline），因为 §5 明确把「心跳超时但
> 重连成功」列为**反面例子**：按连接状态判会把活着的设备标成离线，而调度
> 硬过滤第一步就排除 offline。标准④ 剩下的一半（「连接仍可收发」）成立。

---

## 6. 凭据的下发与节点侧落盘

- 凭据经**已认证的 WebSocket** 下发，节点落盘到工作区内的隐藏位置。
- 节点侧同样收紧权限（0600）；工作区同步时**排除凭据文件**（见 `workspace-and-sync.md` §3.3）。
- **凭据文件绝不上传回 Hub**，也**不进轨迹事件**。

---

## 7. 二进制下发：node 自身升级 与 agent 安装 复用同一机制

两者都是「把一个可执行的东西弄到设备上」，因此共用一套机制：

```text
Hub 侧登记（kind, version, arch）→ blob_hash
    ↓ 心跳：节点上报它有什么、缺什么
Hub 发现缺件 → 回一条 hint { kind, version, blob_hash }
    ↓ 节点主动经 WS 拉 blob（零入站）
校验 SHA-256（内容寻址自带完整性）→ 落盘
    ↓ 加载 → 自检 → 心跳上报就绪；失败只影响这一个件
```

| | node 自身升级 | agent 安装 |
|---|---|---|
| 谁提供代码 | 我们的发布产物 | agent 安装包（同为 CAS blob） |
| 版本策略 | **已定**：立即换 worker，supervisor 懒生效（§4.2） | **未决**，见 `workspace-and-sync.md` §4.4 |
| 身份影响 | 无（身份在 worker，`node_id` 不变） | 无（agent 不是设备身份） |

**共同性质**：零入站（节点主动拉）、失败隔离、可回滚（版本在路径里）。
差异只在「升级策略」——node 是我们的软件可以拍板，agent 是第三方软件必须谨慎。

---

## 7.1 发布与分发：CI 出产物 → 烤进 Hub 镜像 → 节点从 Hub 拉

上一节说「谁提供代码 = 我们的发布产物」。**产物怎么来、怎么进 Hub**，定在这里。
**参考实现：REX**（`/root/workspace/REX`，同构项目：supervisor/worker、心跳版本比对、
多 arch 二进制），本节沿用其已被验证的完整链路，并标注一处必须偏离。

### 7.1.1 CI：多平台矩阵构建 → 全产物喂给两个消费者

**关键设计：构建一次，三个消费者。**

```text
                    ┌─→ Docker Hub 镜像（内嵌全部 arch 的 afcli）
build-binaries ─────┼─→ Docker node 镜像（仅本 arch）
  5 平台矩阵        └─→ GitHub Release + SHA256SUMS
```

| 平台 | runner | 说明 |
|---|---|---|
| linux-amd64 | ubuntu-latest | x86_64 原生 |
| linux-arm64 | ubuntu-24.04-arm | aarch64 **原生**（不用交叉编译——REX 的教训：交叉编 zlib/openssl 会出现架构错配） |
| mac-amd64 / mac-arm64 | macos-latest | 后续 |
| windows-amd64 | windows-latest | 后续 |

**原生编译优于交叉编译**：Rust 静态链 OpenSSL 时，交叉编译的 sysroot 与目标机 glibc
容易错配。GitHub 已有 aarch64 runner，直接用它原生编。

**必须静态链 OpenSSL + zlib**（`OPENSSL_STATIC=1` / `ZLIB_STATIC=1`）：
产出**自包含二进制**，目标机无需装 OpenSSL。树莓派上装 libssl 是真实痛点。
REX 甚至加了 Windows 侧的 `dumpbin /dependents` 断言——exe 里不得出现 `libssl|libcrypto`，
否则说明没链进去。这个断言值得照抄。

前端产物在 CI 里单独 build（`bun install --frozen-lockfile` → `type-check` → `lint` → `test` → `build`），
Rust 侧通过 `include_dir!` / `rust-embed` **编译期嵌入**，运行时不需要 dist 目录。

### 7.1.2 Hub 镜像内置全部 arch 的 afcli（这一节采纳你的提议）

```dockerfile
# Dockerfile.hub
COPY dist/rex-hub        /app/agentfleet          # Hub 自身（已内嵌前端）
COPY dist/nodes/         /app/node-binaries/      # 全部 arch 的 afcli
ENV  AGENTFLEET_BINARIES_DIR=/app/node-binaries
```

- **一个镜像烤全部 arch**（不是每 arch 一个镜像）：`linux/amd64/afcli` + `linux/arm64/afcli` +
  `linux/armv7/afcli`。Rust 单二进制每个 5–15MB，三个合计 15–45MB，**完全可接受**。
- 目录按 `{os}-{arch}` 或 `{os}/{arch}` 组织，REX 两种都兼容（它连 Rust target triple 也认）——
  这个"多格式通吃"的查找函数值得抄，因为它同时兼容手工放置与 CI 放置两种布局。
- **版本耦合自动成立**：Hub vX 只会发 CLI vX，不会出现 Hub 升级了但 CAS 里还是旧二进制的错配。
- **离线/内网友善**：`docker load` 一个 tar，Hub 与 CLI 全有了，全程不碰 GitHub。

### 7.1.3 节点侧：心跳报版本 → Hub 比对 → 推送下载指令 → 经 WS 拉 blob

**REX 的完整链路，AgentFleet 照搬**（这一段已验证可用）：

```text
afcli 心跳上报 node_version
   ↓ Hub 比对自身 version
不一致 → 经 WS 推送 update 指令 { version, download_url, sha256 }
   ↓ afcli 用 reqwest 下载（HTTPS，走 resolve_download_url 拼 Hub 基地址）
   ↓ SHA-256 校验
   ↓ 写 update-state.json { phase: Requested }
   ↓ 优雅退出 exit(10)
supervisor 收到 exit(10) → 读 update-state.json → 原子替换二进制 → 启动新 worker
   ↓ 健康检查通过 → phase: Committed
   ↓ 健康检查失败 → 自动回滚到旧版（phase: RolledBack）
```

`UpdatePhase` 状态机直接沿用（REX 的 13 个阶段）：`Idle → Downloading → Verifying →
Requested → StartingNew → Committed`，失败分支 `RollingBack → RolledBack`，
连续 3 次启动失败 → `Failed`。

**关键：worker 下载、supervisor 替换**。worker 拿到新版二进制后**只写状态文件就退出**，
不自己替换自己——替换由 supervisor 做。这正是约束 12「升级只换 worker、身份不变」的落点。

> **这条链路只适用于 afcli 自身，且是自动的。**
> Hub 比对自己与节点的 `node_version`，不一致就推——**不需要用户在页面上确认**，
> 因为二进制是我们自己的、版本错配会直接破坏协议兼容性（老 node 收不到新 Hub 的必需消息）。
>
> **agent 安装到设备不走这条自动链路**：那必须在页面上由用户显式选择设备 + agent 模板后才下发，
> 且不自动重试、不自动回滚。两条通路的完整对照见 `workspace-and-sync.md` §4.5。
> 把两者混成一条（「发现缺 agent 就自动装」）是**越权修改用户机器的软件构成**，
> 属于产品边界违规，不是实现简化。

### 7.1.4 必须偏离 REX 的一处：节点不能直连 GitHub

REX 的 `download_agent_binary` 在本地找不到二进制时，**回退到 GitHub Releases 下载**
（REX 的做法是 `.../releases/download/v{version}/rex-agent-{os}-{arch}`——设备二进制被它叫做 agent）。

**AgentFleet 不做这个回退。** 理由：
1. **约束 1**：Hub 无入站端口是产品承诺；反过来，让节点去 GitHub 等于给每台设备
   开了条出网依赖，墙内网/离线环境的设备直接卡死在更新上。
2. REX 的 `fallback_url` 与 `sha256` 在指令里**都是空的**——等于跳过校验下载。
   对第三方二进制这不够；对我们的二进制也该走 CAS 的内容寻址校验。

**AgentFleet 的做法**：节点只从 Hub 拉（Hub 镜像内置的二进制就是 CAS 的 seed）。
Hub 侧本地找不到对应 arch 的二进制时，**明确返回 404 + `err{code:"arch_not_supported"}`，
不静默降级到 GitHub**。用户需要给 Hub 补这个 arch 的二进制（放进 `node-binaries/{os}-{arch}/`）。

### 7.1.5 GitHub Release 的定位：规范产物，不是部署路径

Release 仍然保留（CI 自动打 tag 构建 + 软发布 + `SHA256SUMS` + 从 CHANGELOG 抽 body），
但定位是**对外发布的规范产物与构建 provenance**，**不再是节点部署路径上的一环**。
节点升级路径只有一条：`Hub 镜像内置 → CAS → 节点经 WS 拉`。

### 7.1.6 回滚

- 版本在路径里（`node-binaries/{os}-{arch}/afcli-{version}`），回滚 = 换指针，
  不是删文件。
- Hub 侧回滚：把 manifest 指回旧版本。
- 节点侧回滚：supervisor 的健康检查失败自动触发（§7.1.3 的 `RolledBack`）。

### 7.1.7 仍然待定

- **agent（opencode 等第三方）的安装**是否也走「Hub 镜像内置 + CAS」——
  第三方版本发布节奏与我们不同，镜像体积也会涨。**当前仍按 M5 的 TODO 处理**
  （见 `workspace-and-sync.md` §4.4），先不做。
- **armv7（树莓派一代 32 位）**是否需要：取决于是否要支持 32 位老 Pi。GitHub 无原生
  armv7 runner，需要交叉编译或 QEMU。**当前不做**，等真有需求再加。

---

## 8. 不可协商的边界

1. **Hub 永不持有设备私钥。**
2. **节点密钥（`identity.json`）与 agent 凭据永不上传。**
3. **secret 值永不进日志、永不回显、永不进轨迹。**
4. **不加密是本期决定，不是「安全」的声明**——写代码时不要因为「明文安全」而放松其他防护。