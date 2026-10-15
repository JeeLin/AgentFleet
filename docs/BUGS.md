# 缺陷池 / Defect Pool

> 待处理 bug 的汇总地。`mdflow-planner` 在规划里程碑时把 🔴🟡🟢 条目原样搬入里程碑文档的
> `## Bugs` 表，**确认写入行数 == 本表条目数后**才从本表删行（写入与删行成对，缺一即停止）。
>
> Pool of pending bugs. The planner moves 🔴🟡🟢 entries into the milestone's Bugs table at
> planning time, and only deletes rows from here after confirming the move succeeded.

## 待处理 / Pending

| 状态 | 优先级 | 标题 | 来源 | 描述 |
|------|--------|------|------|------|
| 待处理 | 🟡 | `secret` 表明文存储的加固项 | 本项目决定（`architecture/security-and-lifecycle.md` §2.2） | 本期不做应用层加密，需未来补：AES-256-GCM 静态加密 + 独立主密钥；secret 值 mlock 不进 core dump；密钥轮换与失效 |
| ✅ 已关闭 | 🟡 | 分布式推理路线选型 | PRODUCT §5.2 | ~~已评估的可行路径（RPC 层切分 / MoE 分片 / 投机解码）都不直接匹配自托管个人设备场景~~ **已被 AI Gateway 决策取代**：不做自研分布式推理编排，改为把所有平台收敛成统一网关（端点 / 密钥托管 / 按 model·价格·延迟·可用性路由与故障转移 / 额度统计），本地推理作为 `Provider` trait 留口后置。见 `architecture/ai-gateway.md`。保留此行而非删除，是为了留下「为什么这条不再需要评估」的追溯线索 |
| 待处理 | 🟢 | 工作区同步的 B 档 / D 档兜底 | `architecture/workspace-and-sync.md` §3 | Hub bare mirror 兜底（节点连不上 origin）与二进制 delta + 断点续传，当前无里程碑归属，等实测证明需要再排期 |