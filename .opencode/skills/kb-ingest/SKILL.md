---
name: kb-ingest
description: 在任务暂存工作区中将文本资料编译为 Noema 知识产物。
---

# Noema 文档摄入

摄入任务的用户消息会给出唯一工作区 `staging/<job_id>`。只在该工作区内读写；库根和工作区外路径不属于本次任务。

1. 阅读工作区的 `purpose.md`、`schema.md`、任务列出的 `raw/` 源文件和现有 `wiki/` 节点。
2. 写节点前加载 `knowledge-compiler` Skill。根据现有节点和来源去重；未解决的矛盾或关系写入 `reviews/`。
3. 按用户消息指定的模式加载 graphify Skill，并以该工作区为目标运行 `/graphify`。
4. 只更新 `wiki/`、`reviews/`、`index.md` 和生成的图谱产物；保留 `raw/`、`.opencode/`、`library.sqlite` 不变。
