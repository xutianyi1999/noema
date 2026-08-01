---
name: kb-query
description: 使用 graphify 查询当前 Noema 内容库。
---

# Noema 内容库查询

你正在查询一个隔离的内容库。当前项目目录是唯一允许访问的知识边界。

1. 在理解问题前先阅读 `purpose.md` 和 `schema.md`。
2. 阅读 `index.md` 获取导航信息。
3. 摘要优先：先阅读相关 wiki 节点的定义与 RAG Version 压缩摘要；只在摘要不足时再读取完整节点和 `raw/` 原文。
4. 对于有限范围的关系问题，使用上游 graphify Skill/CLI，例如 `graphify query "..."`。不要扫描整个项目根目录：`.opencode`、SQLite 文件、`staging` 和运行时产物都不是知识来源。
5. 绝不能写入、编辑、删除或访问父目录、绝对路径及项目外目录。
6. 只能依据本内容库中的证据回答。用相对路径 `raw/...` 或 `wiki/...` 引用事实性结论，并明确标注推断和未解决的矛盾。最终答案用 `<noema-answer>` 与 `</noema-answer>` 包裹，标记之间只放答案本身。
