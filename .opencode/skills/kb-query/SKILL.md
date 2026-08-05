---
name: kb-query
description: 查询当前 Noema 内容库：摘要优先，按需经上游 graphify Skill 做图谱导航。
---

# Noema 内容库查询

你正在查询一个隔离的内容库。当前目录是内容库根目录，也是唯一允许访问的知识边界。`raw/`、`wiki/`、`reviews/` 和 `graphify-out/` 是知识内容；`staging/` 仅是摄入和维护任务的临时工作区，不是查询来源。

1. 在理解问题前先阅读 `purpose.md` 和 `schema.md`。
2. 阅读 `index.md` 获取导航信息。
3. 摘要优先：先阅读相关 wiki 节点的定义与 RAG Version 压缩摘要；只在摘要不足时再读取完整节点和 `raw/` 原文。
4. 图谱导航：涉及多节点枚举、跨概念关系、或需要先了解库内相关内容版图的问题，调用 OpenCode 的 `skill` 工具加载上游 graphify Skill，按其查询流程定位相关节点，再读取命中节点对应的 wiki/raw 文件（图谱只用于导航，不回写结果）；单一概念的定义或单点事实问题直接读文件即可。
5. 不要扫描整个项目根目录：`.opencode`、SQLite 文件、`staging` 和运行时产物都不是知识来源。绝不能写入、编辑、删除或访问父目录、绝对路径及项目外目录。
6. 只能依据本内容库中的证据回答，并明确标注推断和未解决的矛盾。引用只能指向 `raw/` 原始文档，不得引用 wiki/ 节点——结论经由 wiki 节点找到时，沿其 frontmatter 的 `sources` 回溯到 raw/ 原文再引用。最终答案用 `<noema-answer>` 与 `</noema-answer>` 包裹，标记之间只放符合服务契约的 JSON 对象：完整 JSON Schema、字段要求与格式示例见项目根目录 `AGENTS.md` 的「Noema 服务契约」（由服务生成，与校验器同源）。
