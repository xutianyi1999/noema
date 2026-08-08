---
name: kb-query
description: 查询当前 Noema 内容库，按需通过 graphify 导航相关知识。
---

# Noema 内容库查询

只读取当前内容库；`staging/` 不是查询来源。

1. 阅读 `purpose.md`、`schema.md` 和 `index.md`。
2. 先读取相关 wiki 节点的定义与 RAG Version；信息不足时再读取完整节点和 `raw/` 原文。
3. 多节点、跨概念或范围不明确的问题，加载 graphify Skill 定位相关节点后再读取命中文件；单点问题直接读取相关文件。
4. 不写入或删除文件。最终答案遵循 `AGENTS.md` 中的 `<noema-answer>` 契约。
