# 缺陷池 / Defect Pool

> 待处理 bug 的汇总地。`mdflow-planner` 在规划里程碑时把 🔴🟡🟢 条目原样搬入里程碑文档的
> `## Bugs` 表，**确认写入行数 == 本表条目数后**才从本表删行（写入与删行成对，缺一即停止）。
>
> Pool of pending bugs. The planner moves 🔴🟡🟢 entries into the milestone's Bugs table at
> planning time, and only deletes rows from here after confirming the move succeeded.

## 待处理 / Pending

| 提出版本 | 优先级 | 标题 | 来源 | 描述 |
|----------|--------|------|------|------|
| v0.7.4 | 🟡 | afcli 下载无法选择其他架构 | 用户反馈 | 用户原话「现在的afcli下载是按照当前浏览器的版本下载的 但是我想下载其他架构的怎么半」。**位置**：`packages/web/src/domains/device/DevicesPage.vue:121`、`packages/web/src/domains/device/api.ts:65-100`。**证据**：页面把 `detectDownloadPlatform(navigator.userAgent, navigator.platform)` 的推断结果直接拼进 `GET /api/agent-binary?os=&arch=`，**没有任何平台选择控件**；而后端 `crates/hub-server/src/routes/agent_binary.rs` 的 `?os=` / `?arch=` 已是闭集，且 `Dockerfile.hub` 已嵌入 Windows amd64 / macOS arm64 / Hub 架构 Linux 三份 afcli。**断言**：用户在 Windows 上想取 Linux 版、或在 x86_64 Mac 上想取 arm64 版时，页面只能给浏览器自身平台，没有任何入口能改；后端能力已存在但前端不暴露。**处置**：用户 2026-11-11 裁定加入缺陷池、下个版本处理（v0.7.4 不加页面改动，见里程碑「无页面 / 原型重做」核对点）。 |
| v0.7.4 | 🔴 | Windows 上 afcli 连 Hub 握手拿到 HTTP 200，设备永远注册不上 | 用户反馈 | **位置**：`crates/node-core/src/conn.rs`（WS 握手）→ 错误文本来自底层 WebSocket 客户端对非 101 响应的 `Error::Http`。**证据**：Windows 上 `.\afcli-windows-amd64.exe run --foreground`，Hub `ws://124.222.106.241:8000/ws`，数据目录 `C:\Users\jjee\AppData\Roaming\agentfleet`，重试 1–5 全部 `connect failed: connect failed: HTTP error: 200 OK`。**断言**：`/ws` 返回的是 HTTP 200（通常是 SPA fallback 的 HTML 或反向代理的默认页）而不是 `101 Switching Protocols`，设备因此永远注册不上；这既可能是部署形态（代理 / 端口 / 路径 / Hub 版本）问题，也可能是 Hub 路由未命中。**处置**：需先复现定位（是代理还是路由），定位后再定修复点；与 v0.7.4 的会话导入无关，不占用本里程碑范围。 |

