---
name: noema-cli
description: 使用 noema-cli 管理和操作 Noema 内容库：检查服务、创建和列出内容库、导入导出快照、从本地 Markdown/TXT 文件摄入、轮询摄入作业，以及查询内容库。处理法规或任务 workspace 中的文件并写入 Noema 时必须使用此 Skill；文件正文不能通过 MCP 工具或命令行参数传递。
---

# Noema CLI

使用 `noema-cli` 操作 Noema HTTP 服务。默认服务地址是
`http://127.0.0.1:8787`；受鉴权保护的服务从 `NOEMA_AUTH_TOKEN` 读取令牌。
需要连接其他服务时传 `--server <URL>`，也可由 `NOEMA_SERVER` 配置。不要把令牌
写入命令历史。`--json` 仅为 `submit` 和 `job` 提供机器可解析输出。

## 基本操作

```bash
noema-cli status
noema-cli create <name> [--description <text>]
noema-cli list
```

`create` 只用于新内容库。若调用方需要“存在则复用”，先用 `list` 明确确认，
不要把 `create` 的冲突当作可忽略错误。

## 快照

```bash
noema-cli export <library> --output <archive.tar.gz>
noema-cli import <archive.tar.gz> [--name <name>] [--description <text>]
```

`import` 总是创建新库；导入前确认名称和归档来源。`export`/`import` 处理的是
归档文件，不能把归档内容作为命令参数。

## 文档摄入

先在任务 workspace 中生成完整 UTF-8 `.md` 或 `.txt` 文件。Markdown 的文件名必须是
预期的 Noema 文件名；不要将正文复制进提示词、MCP 参数或 shell 参数。

```bash
noema-cli --json submit <library> <file-1.md> [<file-2.md> ...]
```

一次 `submit` 会创建一个覆盖所有非跳过文档的作业。仅提交一个文件时，才可增加
`--title <title>`。记录返回 JSON 中的 `library_id`、`job_id` 与每项 `documents` 状态；
不能把命令已发出当作摄入完成。

轮询作业直到终态：

```bash
noema-cli --json job <library> <job_id>
```

只有 `completed` 或 `skipped` 是成功终态；`failed` 必须报告错误并让调用任务失败。
轮询间隔至少 2 秒，并使用与任务时限一致的总超时。

对于法规导入任务：下载输入 artifact 后校验其 `sizeBytes`，再用适用的文档提取 Skill
生成 `converted/<noemaFilename>`。在调用 `submit` 前必须完成全部文件的下载、转换和
非空 UTF-8 校验；任一文件失败就结束整个任务，不得提交任何文件或提交子集。全部成功后
才将全量文件放进同一次 `submit`。作业成功后发布 `batch/report.json`，并通过
`lexifact-agent-runtime` 上报相同的 `jobId`、`jobStatus`、`fileStatuses` 和
`documentMetadata`。报告中的每个文件必须有 `noemaFilename`、`status` 和 `sizeBytes`。

## 查询

```bash
noema-cli query <library> "<prompt>" [--session-id <session_id>]
```

初次查询会输出 session id；后续问题传相同的 `--session-id` 延续会话。OpenCode
不接入 Noema MCP，所有内容库操作都使用本 Skill 的 CLI 工作流。Noema 保留独立的
标准 MCP 服务给其他客户端，但它不提供 `kb_ingest_documents`。
