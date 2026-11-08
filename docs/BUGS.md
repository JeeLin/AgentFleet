# 缺陷池 / Defect Pool

> 待处理 bug 的汇总地。`mdflow-planner` 在规划里程碑时把 🔴🟡🟢 条目原样搬入里程碑文档的
> `## Bugs` 表，**确认写入行数 == 本表条目数后**才从本表删行（写入与删行成对，缺一即停止）。
>
> Pool of pending bugs. The planner moves 🔴🟡🟢 entries into the milestone's Bugs table at
> planning time, and only deletes rows from here after confirming the move succeeded.

## 待处理 / Pending

| 提出版本 | 优先级 | 标题 | 来源 | 描述 |
|----------|--------|------|------|------|
| v0.7.3 | 🟡 | Hub 镜像缺少 git，基础镜像方案待评估 | 用户反馈 | 现在的hub镜像里没有git吧？是否改成继续debian13镜像为基础加装git |
