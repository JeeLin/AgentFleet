# 架构决策记录

> 记录"为什么这么定"，避免改着改着忘了当初的取舍。
> 每条含**决策 / 理由 / 被否决的替代方案**。未决项标注 ⬜ 待定。

---

## D1. 设备端进程模型：同二进制双角色（supervisor / worker）

**决策**

```
afcli run                 → supervisor（默认角色，父进程）
  └─ spawn: afcli run --worker    ← 同一个二进制，只多一个 flag
       └─ 持有 identity.json + WS 连接 + 心跳 + 重连
```

- **supervisor 不碰 WS、不碰业务**。唯一职责：保证有一个能用的 worker。
- **回滚由 supervisor 执行**，而 supervisor 是**没被替换过的旧代码**。
- Docker 模式：`afcli run --worker` 直接跑 worker，无 supervisor、无特判分支。

### 为什么是 `run --worker` 而不是顶层 `--worker` 或环境变量

| 形式 | 评价 |
| --- | --- |
| `afcli run --worker` ✅ | `run` 是"启动守护进程"这个动词，角色是它的修饰符。supervisor spawn 时**把自己的 argv 原样传下去再追加 `--worker`**——用户给的 `--data-dir` 之类参数自动继承，不用重新拼一遍 |
| `afcli --worker` | 没有动词，与 `afcli log` / `afcli status` 这类子命令不一致；看起来像全局开关，实际只影响 `run` |
| `AFCLI_WORKER=1` 环境变量 | ❌ **不采用**：(a) `ps` 里看不见角色，排查"这进程是 supervisor 还是 worker"得翻 `/proc/<pid>/environ`；(b) 环境变量会被孙进程继承，worker 再 spawn 任何东西都会误以为自己是 worker；(c) 与 argv flag 并存就是**两条代码路径**，而覆盖率门禁要求 100%，多一条路径就多一份要单独构造的分支 |

**只有一条进入 worker 角色的路径**：argv 里含 `--worker`。
（原设计同时保留 env var，现删除——两条路径不增加表达能力，只增加要测的分支。
REX 用 env var 是因为它 spawn 时只透传原始 argv；追加一个 flag 一样能做到，还顺带可见。）

**幂等**：若 supervisor 自己就是带 `--worker` 启动的（人手误敲、或 Docker 里直接指定），
它**不再追加**，直接以 worker 角色运行——不会出现 `run --worker --worker`。

**理由**

「执行回滚的代码不参与被回滚」——新二进制再坏也坏不到 supervisor 里，
坏 worker 只会 `exit(11)`，supervisor 照样能把它换回去。
supervisor 职责极小 ⇒ 没有 bug 面 ⇒ 恰恰是最需要永远可靠的那个进程。

**被否决的替代方案**

| 方案 | 否决原因 |
| --- | --- |
| A. supervisor 持 WS，IPC 转发给 worker | 要写一层帧转发 + req_id 映射表，M0 worker 是空壳，为不存在的转发需求付 100% 覆盖率的账 |
| B. worker 持 WS，supervisor 只拉起 | 方向对，但不如双角色彻底——仍需处理 WS 生命周期 |
| 单进程 + `execv` 原地换映像 | **根本性缺陷**：新二进制自己回滚自己。为"同 PID"这个假收益，放弃了唯一真正重要的属性 |

**参考实现**：REX（`rex-common/src/supervisor.rs`）。本方案在其基础上修了 6 个缺陷，
见 [D2](#d2-升级与回滚时序)。

---

## D2. 升级与回滚时序

**决策**：版本号**由 Hub 通过 WS 下发**，不自己探测 GitHub Releases。

```
Hub ──heartbeat 应答带 update{version,url,sha256}──► worker
                                                      │ 下载 → SHA256 校验
                                                      │ 写 update-state.json (phase=Requested)
                                                      │ exit(10)
                                                      ▼
supervisor: 备份 exe → .bak  →  rename(staging → exe)  →  phase=StartingNew
                                                      │
                                             启动新版 worker
                                                      │
                              健康检查：30s 内完成握手且 node_version == 目标版本
                                                      │
                                    失败 → exit(11) → supervisor rollback()
```

- **worker 立即生效新代码**；**supervisor 懒生效**——文件已换成新的，进程继续跑旧映像，
  下次被 systemd/docker/用户拉起才更新。
  - 前提（**显式承诺**）：supervisor 的 ABI 是极小的稳定接口 = `spawn` + 退出码协议 + `update-state.json`，必须向后兼容。
  - 附带解决 Windows：没有 systemd 兜底，懒生效让 supervisor 永远不必为了自己的新代码而退出。

### 对 REX 参考实现的 6 处修正

| # | REX 的问题 | 本项目的修法 |
| --- | --- | --- |
| ① | `fast_crash` 判定在读 update-state **之前** → 坏版本把 supervisor 直接拖死，**永不回滚**（砖设备） | 判定移到读 state **之后**；本轮有 pending update → 走 rollback 而非 exit |
| ② | 退出码 0 不算 fast_crash → 「启动即干净退出」的 worker 以 1s 间隔无限重生，**永不回滚**，Hub 看到心跳风暴 | 改为**时间窗内连续退出计数**，与退出码无关（10s 内 3 次 → 不可恢复 → rollback） |
| ③ | 备份失败后仍 rollback → 用**上一轮**遗留的 `.bak`，**降级到几个月前的陈旧版本**且用户不知情 | **备份先行且必须成功**才进下一阶段；state 记 `generation`，rollback 只认本轮备份，对不上 → `phase=Failed` 大声报错 |
| ④ | 错误全线静默（`let _ = rename`、三层 `if let Ok`、`.ok()`）→ 磁盘满时后续阶段全错，日志一行没有 | 所有 I/O 失败必须 `tracing::error!`；**state 写失败 = 放弃本次更新**（宁可不升级，不可进到"说一套做一套"的阶段）。本项目 `let_underscore_must_use` / `unwrap_used` 是 deny，这类代码本来就编不过 |
| ⑤ | 回滚防死循环是 rename 消耗掉 `.bak` **偶然形成**的，无文档、无计数 | 显式 `rollback_count ≥ 1` → `phase=Failed` + 大声报错，不依赖 rename 副作用 |
| ⑥ | 崩溃在 `rename(exe)` 与 `write StartingNew` 之间 → 重启后 state=Requested 但 staged 已挪走 → **误回滚一次已成功的更新** | supervisor 启动时 **reconcile**：staged 不存在 && 二进制已是目标版本 → `phase=StartingNew` 继续；否则 `phase=Failed` |

另：`std::process::exit()` 不跑 drop，`tracing` 最后几行日志会丢 → 改为从 `main` 返回。

### 决策逻辑必须纯函数化

```rust
fn decide(exit: ExitInfo, state: Option<UpdateState>) -> Decision   // 纯函数，全分支单测
enum Decision { ReplaceAndSpawn, Rollback, Spawn, BackoffSleep, Stop }
// executor 只执行 Decision —— 薄到不需要单测，靠集成测试覆盖
```

**这不是洁癖，是被 100% 覆盖率门禁逼出来的**：REX 把决策内联在阻塞循环里，
里面真的 spawn 子进程、真的 rename 文件——在 100% 行覆盖下几乎没法测（测了就真删二进制）。

---

## D3. 升级触发方式与可见性

**决策**

- Hub 页面**显示每个设备的 CLI 版本号**
- 页面可**手动触发升级**
- 页面提供**自动升级开关**（开 / 关）

**理由**：设备状态必须在 Web 上"一眼可见"（产品基调）。升级失败必须在页面上看到
「设备 pi-1 升级失败，已回滚」，而不是只能 ssh 上去看日志。

**被否决 / 明确不做**

- ❌ 最短发布年龄（24h）—— 升级跟 Hub 走，Hub 自己决定何时推
- ❌ 不降级检查 —— Hub 不会下发比自己旧的版本
- ❌ 包管理器检测 —— 不进包安装体系（暂不考虑进包安装）

**协议影响**：需新增 `update_available{version,url,sha256}` 消息类型
（`protocol::message::types`，非必需集，老节点忽略）。同步更新 `docs/architecture/protocol.md` §2。

**验签分层**（此处修正此前的高估）：
- **M0：SHA256**（Hub 下发，防下载文件被污染 —— URL 走 CDN，这是真实威胁）
- ❌ ed25519 全量验签随 release 流水线一起立，**不阻塞 M0**
- 理由：信任根就是 Hub，Hub 本就能下发任意命令，签名防不住 Hub 被攻破，边际价值小

---

## D4. 日志：`afcli log`

**决策**

- 日志落盘：`<data-dir>/logs/afcli.log`，按大小 / 天数轮转，权限 **0600**
- supervisor 与 worker **写同一个文件**（追加，整行原子写）
- `afcli log` 默认 tail 最近 **200 行**；`afcli log -n <N>` 指定行数
- 所有 I/O 失败必须留痕（同 D2-④）

**理由**：`afcli` 是守护进程，**没有日志等于黑盒**——升级失败、回滚、重连退避这些
只在设备端发生的事件，用户 ssh 上去也看不到。这条把"日志落地"从 M1 拉进了 M0。

---

## D5. Hub 存储后端：sqlite 默认 / postgres 可选

**决策**（新立里程碑，**不并入 M0**）

- Hub 自身存储的**完整后端**：`sqlite://` 默认、`postgres://` 可选
- 导出为**一次性快照**（不是持续同步）
- 不引 Redis；不做多节点 Hub（多用户需求以后再说）

---

## D6. Windows 支持范围 ⬜ 待定

- **node 侧必须支持 Windows**（cli 要在 Windows 上，后续还要能操作电脑）
- **Hub 是否也要跑 Windows**：⬜ 未决。**建议 B：Hub 仍 Linux**（单用户 self-host，
  部署在 NAS/小服务器，无 Windows 诉求）

**Windows 改造面**（现有硬 Unix 依赖，供估算）：

| 依赖 | 位置 |
| --- | --- |
| `libc::umask(077)` | hub-storage/db.rs、main.rs |
| `PermissionsExt::mode(0o600)` | db.rs、identity.rs、setup.rs |
| `libc::kill(SIGTERM)` | afhub_process.rs |
| `libc::execv` | 自更新 |
| `/proc/net/tcp` + `/proc/<pid>/fd` | ws_flow.rs |

⚠️ **约束 13 的「0600/0700/umask 077」在 Windows 上无语义等价物，需按 NTFS ACL 重定义。**

⚠️ **`self_replace` 在 Windows 的已知坑**（Mise #12062）：先移走 exe 再从 TEMP 启 helper，
路径超 `MAX_PATH`(202) 时 `CreateProcess` 失败 → **安装目录变空、设备变砖**。上 Windows 必须预检路径长度。

---

## D7. Origin 检查

**决策**：保留，定位为**纵深防御，不是安全边界**。
（曾提议改用自定义 User-Agent —— ⬜ 未落地，未被否决）

---

## 已删除 / 边界裁定

- 删死代码 `open_default`
- 「发现设备缺 agent 就自动装」= **产品边界违规**（`afcli` 是接入通道，不是软件分发器）
- Gateway 配置同步是**追加**不是**替换**
- `docs/BUGS.md`「分布式推理路线选型」条目已被 ai-gateway 决策取代，属失效条目待清理
