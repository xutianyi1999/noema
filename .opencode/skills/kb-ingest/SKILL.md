---
name: kb-ingest
description: 将文本资料摄入当前隔离的 Noema 内容库。
---

# Noema 文档摄入

当前目录是内容库根目录。每个摄入任务的用户消息会给出唯一的工作区 `staging/<job_id>`；该目录是本次任务的完整副本，也是唯一可读写的知识工作区。库根的 `raw/`、`wiki/`、`reviews/`、`graphify-out/` 是正式内容，不能直接修改。

1. 先在任务工作区内阅读 `staging/<job_id>/purpose.md` 和 `staging/<job_id>/schema.md`。
2. 阅读摄入提示中给出的任务工作区内原始资料路径（可能不止一篇：失败重试的任务会要求补编译此前未落盘的源文档）。创建新概念前，先检查该工作区内现有的 `wiki/` 节点。
3. 服务已将任务输入复制到 `staging/<job_id>/raw/`。
4. 必须使用 OpenCode 的 `skill` 工具加载上游 graphify Skill，并以任务工作区为目标：首次建图执行 `/graphify staging/<job_id>`；增量更新执行 `/graphify staging/<job_id> --update`。绝不能执行 `/graphify .`，那会指向内容库根目录。任务工作区内的 `.graphifyignore` 已把输入限定为其 `raw/` 和 `wiki/` 下的 Markdown/TXT；不要扫描 `.opencode`、SQLite 或其他运行时文件，也不要把 graphify 当成可选步骤或只运行裸 `graphify update .`。
5. 每个节点只表达一个核心概念。使用包含稳定 `node_id`、`canonical_name`、`kind`、`sources`、明确关系、`claim_type`、`confidence`、`created_at` 和 `updated_at` 的 YAML frontmatter。`sources` 的 locator 按来源自身编号标注（如 第三十三条第二款、5.2.1）。
6. 每个节点都要包含定义、证据/推理、示例或反例、局限性、RAG Version 和引用。法条、合同条款、公文与标准原文必须逐字引用，RAG Version 只压缩评述与关系，不改写规范文本。
7. 如果概念已经存在，就更新已有节点。将冲突、重复、不确定关系和新旧文本矛盾放到 `reviews/`，用 `opposite_to` 表达；绝不能把推断写成事实。
8. 只在任务工作区中更新 `index.md` 以及生成的图谱/索引产物。绝不能修改任务工作区中的 `raw/`、`.opencode/`、`library.sqlite`，也绝不能修改内容库根目录或项目外路径。
