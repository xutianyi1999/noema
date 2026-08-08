---
name: kb-maintain
description: 在任务暂存工作区中维护 Noema 知识产物。
---

# Noema 内容库维护

维护任务的用户消息会给出唯一工作区 `staging/<job_id>`。只在该工作区内读写；库根和工作区外路径不属于本次任务。

1. 阅读 `purpose.md`、`schema.md` 和受变更影响的知识节点。
2. 重新对齐来源、关系和节点：删除仅由已删除来源支撑的节点，保留并更新共享节点；将无法消解的问题写入 `reviews/`。
3. 按用户消息指定的模式加载 graphify Skill，并以该工作区为目标运行 `/graphify`。
4. 只更新知识产物，保留 `raw/`、`.opencode/` 和 `library.sqlite` 不变。
