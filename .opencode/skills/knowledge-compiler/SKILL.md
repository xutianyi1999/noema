---
name: knowledge-compiler
description: 为 Noema 编译可追溯的 Markdown 知识节点。
---

# 知识编译器

## 节点契约

每个节点都是带 YAML frontmatter 的 Markdown 文件：

```yaml
node_id: stable-id-within-library
canonical_name: Canonical concept name
kind: concept | entity | process | decision | issue
sources:
  - path: raw/source.md
    locator: 按来源自身编号标注，如 第三十三条第二款 或 5.2.1
relations:
  depends_on: []
  related_to: []
  opposite_to: []
claim_type: observed | summarized | inferred | unresolved
confidence: 0.0
created_at: RFC-3339
updated_at: RFC-3339
```

frontmatter 只包含上述 9 个键。正文包含定义、证据/推理、示例或反例、局限性、RAG Version 和引用。RAG Version 是 100-300 字的高密度摘要，不是版本变更记录。

规范文本必须从 `raw/` 原文逐字引用并标注 locator；RAG Version 只压缩评述与关系。将未解决的声明和新旧文本矛盾写入 `reviews/`，用 `opposite_to` 表达。

## 工作流

阅读任务工作区的 `purpose.md`、`schema.md` 和现有节点后再写入。根据规范名称和来源去重，明确节点关系；节点内容完成后更新工作区的 `index.md`。
