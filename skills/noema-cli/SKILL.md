---
name: noema-cli
description: 使用 noema-cli 管理 Noema 内容库、文档、作业、快照和查询；向 Noema 写入文件时使用此 Skill。
---

# Noema CLI

默认服务地址为 `http://127.0.0.1:8787`。使用 `NOEMA_AUTH_TOKEN` 鉴权；需要其他服务时传 `--server <URL>` 或设置 `NOEMA_SERVER`。`--json` 为文档、提交和作业命令提供机器可读输出。

## 内容库与文档

```bash
noema-cli status
noema-cli create <name> [--description <text>]
noema-cli list
noema-cli --json documents <library>
noema-cli download <library> <filename> --output <local-file.md>
```

`create` 创建新库；需要复用已有库时先用 `list` 查找。`documents` 返回文档元数据，`download` 将该文档的 `raw/` 原文写到本地文件。

## 快照

```bash
noema-cli export <library> --output <archive.tar.gz>
noema-cli import <archive.tar.gz> [--name <name>] [--description <text>]
```

`import` 总会创建新内容库。

## 文档摄入

先在任务工作区中准备 UTF-8 `.md` 或 `.txt` 文件，再一次性提交本批文件：

```bash
noema-cli --json submit <library> <file-1.md> [<file-2.md> ...]
noema-cli --json job <library> <job_id>
```

记录 `submit` 返回的 `library_id`、`job_id` 和逐文件状态，并轮询作业至终态。`completed` 与 `skipped` 表示成功；`failed` 使调用任务失败。单文件提交时可附加 `--title <title>`。

## 查询

```bash
noema-cli query <library> "<prompt>" [--session-id <session_id>]
```

首次查询返回 session id；后续查询可传同一 `--session-id` 继续会话。
