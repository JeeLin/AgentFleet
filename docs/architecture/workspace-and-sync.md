# 工作区供料与同步 / Workspace Feeding & Sync

> 问题：把「一个完整任务」分发到某台设备执行时，项目文件、技能配置、最终产物
> 这三样东西怎么从 Hub 到设备、又怎么回来？
> 特别地：项目不是 git 项目怎么办？仓库很大怎么办？设备网络差连不上 origin 怎么办？

**核心原则一句话：不同步「文件夹」，同步「内容」。**
Hub 是唯一真相源，一个内容寻址仓库（CAS）统一供料，三样东西共用同一个 blob store，
只是寻址方式不同。

> Feed devices with **content**, not with a mirrored folder tree.
> Hub is the single source of truth; one content-addressed store serves all three needs.

---

## 1. 三个用途，一个 CAS

```text
                  ┌────────── Hub CAS（SHA-256 + zstd）──────────┐
                  │   blob / tree / manifest / snapshot / artifact   │
                  └───┬──────────────────┬──────────────────┬─────┘
      按 (node, path) │  按 (kind, version)│        按 artifact_id
        物化工作区    │    铺设配置        │        归档产物    │
```

| 用途 | 寻址 | 期望值语义 |
|---|---|---|
| 项目文件 | `(node_id, path)` + snapshot | **任务级、临时**：每个任务一份期望树 |
| 技能 / agent 配置 | `(kind, name, version)` | **节点级、持久**：节点上恒定保持 Hub 的最新版本 |
| 产物 | `artifact_id` | **任务级、永久保留**：设备关掉后仍可取回 |

关键差异：项目文件几乎不会冲突（工作区独占），配置**会**（用户可能在节点上改过），
所以配置走「托管覆盖层」而不是「强制覆盖」。

---

## 2. WebSocket 是控制通道，不是数据通道

大文件尽量不走节点↔Hub 的 WebSocket。WS 承载：控制指令、manifest、
终端字节流、产物下载。**仓库内容不进 WS。**

---

## 3. 项目文件怎么到设备：按情况分四档

| 档 | 情况 | 做法 | 进 WS 的字节数 |
|---|---|---|---|
| **A** | git 项目，节点能连 origin | Hub 只发 `repo_url + commit_sha`，节点自己 `git fetch` + `worktree add` | 几百字节 |
| **B** | git 项目，节点连不上 origin | Hub 跑一个 bare mirror（Hub 自己拉），节点从 Hub 拉 | git 增量 pack，通常几百 KB |
| **C** | 非 git 项目 | Hub 存 CAS 快照，节点按 manifest 差分物化 | 变更文件全量（zstd 压缩） |
| **D** | 单个大文件 / 网络很烂 | 在 C 之上加二进制 delta + 断点续传 | 只传差异块 |

**A 是主路径**，绝大多数真实项目走 A。这是 git 分布式设计白送的红利，不要浪费。
B / D 是兜底，等实测证明需要再实现。

### 3.1 C 档机制：manifest 差分

```text
节点侧：遍历工作区 → 上报 manifest { path, size, sha256 }      # 几十~几百 KB
Hub 侧：diff(目标 manifest, 节点 manifest) → 变更指令列表
节点侧：按列表拉 blob / 删除多余文件 → 回报新 manifest
```

**先做「全文件传输 + 内容哈希跳过」，不要一上来就做二进制 delta。**
内容哈希已经消掉绝大部分重复；只有实测发现大文件（模型权重、数据集、编译产物）
传输成为瓶颈，才加 D 档（`zstd --patch` 之类）。

### 3.2 同一台设备上多任务并行

用 **git worktree** 隔离，不复制目录：共享 object store，不重复占磁盘。

```bash
git worktree add ../wt/task-123 <commit_sha>
```

### 3.3 永不传输清单

同步必须带排除策略，否则 `node_modules` / `.venv` / `target/` 会把网络打死：

```text
node_modules/  .venv/  venv/  target/  dist/  build/  .cache/
*.o  *.so  *.rlib  *.pyc  .git/objects/pack/tmp
```

- git 项目：直接读 `.gitignore`。
- 非 git 项目：默认列表 + 用户可追加。
- **依赖在设备本地重建**，不进同步；重建步骤作为 checkpoint 的一步记录，可断点续跑
  （见 `task-continuity.md`）。

---

## 4. 技能与 agent 配置铺设：托管覆盖层

技能、`opencode.json`、子 agent 定义这类东西**不是项目文件**，是节点环境的一部分。

### 4.1 三条规则

1. **Hub 记录托管清单**（`managed_file` 表：路径 + 内容哈希 + 来源 bundle）。
   升级时只动清单内的文件，能改、能删、能回滚。
2. **托管文件在节点上设为只读。** agent 要改会得到警告，用户要改会看到「这是 Hub 托管的」。
3. **漂移检测**：心跳顺带报托管文件的实际哈希，对不上就在 Hub 上显示
   「N 个文件漂移」，给 push / pull。**不自动覆盖**——用户可能是故意改的。

### 4.2 权限配置：全局层 + 会话层

agent 用它自带的权限机制（Claude Code / OpenCode 的 allow/ask/deny）。
**我们不实现权限引擎，只负责下发配置**——而配置由我们托管，因此可统一管理。

两层，后者覆盖前者：

```text
全局层（Hub 统一管，所有节点所有会话生效）
  permissions = {
    "rm -rf /*":   "deny",
    "git push --force": "ask",
    "sudo *":      "ask",
    "curl *":      "ask"
  }

会话层（创建会话时可临时加严，不可放松全局的 deny）
  permissions += { "npm publish *": "ask", "rm -rf /tmp/*": "deny" }
```

- **会话层只能加严，不能放松全局 deny**——避免「开个会话就绕过全局禁令」。
- 规则**在派发子任务时随任务一起下发**，子 agent 继承并可再加严。
- **这是自律，不是强制**：真正的强制边界是操作系统权限（agent 以什么用户跑、能碰哪些目录）。
  我们控制工作区目录与运行用户，不做额外沙箱（见 `architecture/README.md` §4.2）。

### 4.3 目录划分

```text
节点上 ~/.config/opencode/：
  agents/*.md      ← Hub 托管（只读）：子 agent 定义
  skills/*.md      ← Hub 托管（只读）
  opencode.json    ← Hub 托管（只读）：模型 / 权限 / MCP server 配置
  auth.json        ← 节点所有：Hub 永不读、永不上传
  其他             ← 节点所有：Hub 不碰
```

模型 API key（LLM 用）在 Hub 的托管配置里统一定义，下发给各节点复用。
凭据的存储取舍见 `security-and-lifecycle.md`。

### 4.4 agent 软件本体：Hub 不负责安装

本项目的分发**只覆盖配置**（skills / agents / 权限 / 模型 key），**不覆盖 agent 软件本体**。

不提供「在 Hub 点一下就把 agent 装到新机器并配好」的一键部署。

理由：agent 怎么装、装哪个版本，是设备环境的一部分，与项目文件同步是两件事；
而「配置同步」已经由上面的托管覆盖层解决。用户在设备上自行安装 agent，
Hub 侧只管理它的配置。

---

## 5. 产物归档

任务产出的最终制品按 `artifact_id` 存进同一个 CAS，挂上元数据；
轨迹里记 `artifact_ref`，点开直接从 Hub 下载，**不需要回原设备**（设备可能已关机）。

保留策略三档：临时（会话内）/ 归档（默认）/ 永久（显式标记）。

---

## 6. 明确不做 / Non-goals

| 别做 | 为什么 |
|---|---|
| Syncthing 当主同步通道 | 需要外部 relay；Hub 不再是真相源；会话级工作区难以推理 |
| 设备间 P2P 直传 | 破坏「Hub 是唯一真相源」；认证模型复杂化；手机 NAT 下还需中继 |
| 自研流式同步协议 | git / rsync / zstd 已解决且可审计 |
| 同步 `node_modules` 等依赖 | 体积爆炸、平台相关、可能含绝对路径 |

---

## 7. 数据表（初版）

```sql
blob(hash TEXT PK, size_bytes INTEGER, refcount INTEGER)   -- 内容寻址，zstd 压缩
snapshot(id TEXT PK, created_at TEXT, manifest TEXT)       -- manifest = JSON 路径→hash
artifact(id TEXT PK, task_id, session_id, node_id TEXT,
         name, mime, size_bytes INTEGER, blob_hash TEXT,
         produced_at TEXT, retention TEXT)                 -- temp / archived / keep
managed_file(node_id, path, blob_hash, bundle_id TEXT,
             PRIMARY KEY(node_id, path))                   -- Hub 托管清单（配置铺设）
```

- `refcount` 用于回收：无快照 / 无产物 / 非托管引用的 blob 可清理。
- SQLite：建库必须 `journal_mode=WAL` + `busy_timeout`。

---

## 8. 版本 ≠ 功能

节点与 Hub 有 `protocol_version`，但**功能由能力协商决定，不由版本号决定**：

- 心跳上报节点支持的 feature 列表，Hub 按能力下发，不按版本号猜测。
- 协议保持向后兼容：新增消息类型可选，老节点忽略未知字段。
- **不维护「每个版本对应一套功能」**——那是维护地狱，且必然漏。

不兼容时明确报错并提示升级，**不静默失败**。