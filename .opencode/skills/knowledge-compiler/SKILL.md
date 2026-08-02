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

frontmatter 只包含上述 9 个键，不要添加额外键。正文必须包含以下小节：

- 定义：准确、非循环的定义
- 证据/推理：支撑结论的关键证据，附来源定位
- 示例或反例
- 局限性：来源未覆盖或未确认的部分
- RAG Version：本节点 100–300 字的高密度压缩版本，保留核心推理链，适合直接注入 LLM 上下文；不是版本变更记录
- 引用：来源文档与相关节点的相对路径，如 `raw/example.md`（第三十三条）或 `wiki/concept.md`

`node_id` 是一个 `library_id` 内的身份标识；文件名可以变化。

法条、合同条款、公文与标准原文等规范文本必须从 `raw/` 原文逐字引用并标注 locator（按来源自身编号，如 第三十三条第二款 或 5.2.1），不得改写；RAG Version 只压缩节点的评述与关系，不改写规范文本。新旧文本矛盾、未解决的声明放入 `reviews/`，用 `opposite_to` 表达。

## 工作流

写入前阅读 `purpose.md`、`schema.md` 和现有节点。保留原始资料的出处。根据已有规范名称和来源去重。明确表达节点关系。将未解决的声明和矛盾放入 `reviews/`。只有在节点文件内容一致后才更新 `index.md`。不要使用自定义知识库 MCP 工具。
