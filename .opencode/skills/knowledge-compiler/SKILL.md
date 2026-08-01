---
name: knowledge-compiler
description: 为 Noema 编译可追溯的 Markdown 知识节点，不照搬 LLM-WIKI 工作流。
---

# 知识编译器

这是一个原生面向 OpenCode 的知识编译器，借鉴持久化 Wiki 的思想。它是独立的 Noema Skill，不依赖 Claude 工具、URL 阅读器、上游 LLM-WIKI 目录或上游命令名称。

## 节点契约

每个节点都是带 YAML frontmatter 的 Markdown 文件：

```yaml
node_id: stable-id-within-library
canonical_name: Canonical concept name
kind: concept | entity | process | decision | issue
sources:
  - path: raw/source.md
    locator: heading or line range when available
relations:
  depends_on: []
  related_to: []
  opposite_to: []
claim_type: observed | summarized | inferred | unresolved
confidence: 0.0
created_at: RFC-3339
updated_at: RFC-3339
```

正文必须包含准确的定义、证据/推理、示例或反例、局限性、RAG Version 和引用。`node_id` 是一个 `library_id` 内的身份标识；文件名可以变化。

## 工作流

写入前阅读 `purpose.md`、`schema.md` 和现有节点。保留原始资料的出处。根据已有规范名称和来源去重。明确表达节点关系。将未解决的声明和矛盾放入 `reviews/`。只有在节点文件内容一致后才更新 `index.md`。不要使用自定义知识库 MCP 工具。
