# Noema

Noema 是由 OpenCode 驱动的文本知识库服务。服务负责内容库、原文、任务和对外协议；OpenCode 负责读取工作区、调用 graphify、编译知识节点并回答查询。

每个内容库都有独立的 OpenCode project、原文、Wiki 节点、图谱产物、索引和 SQLite 文件。查询只接受自然语言 prompt，每次查询都会创建一个新的 OpenCode session。

## 依赖

- Rust stable
- 可访问的 OpenCode Server，默认地址 `http://127.0.0.1:4096`
- `graphify` CLI。创建内容库时默认执行 `graphify install --platform opencode --project`
- 本地 OpenCode SDK：`/mnt/data/code/agentic_auxilary/crates/services/opencode-rs`

## 启动

先启动 OpenCode Server，再启动 Noema：

```bash
OPENCODE_TEST_MODEL=opencode/deepseek-v4-flash-free \
NOEMA_DATA_DIR=data \
cargo run
```

常用配置：

| 环境变量 | 默认值 | 作用 |
| --- | --- | --- |
| `NOEMA_BIND` | `127.0.0.1:8787` | Noema 监听地址 |
| `NOEMA_DATA_DIR` | `data` | 服务数据目录 |
| `OPENCODE_URL` | `http://127.0.0.1:4096` | OpenCode Server 地址 |
| `OPENCODE_TEST_MODEL` | `opencode/deepseek-v4-flash-free` | 优先使用的模型标识 |
| `OPENCODE_TIMEOUT_SECS` | `1800` | 单个 Agent session 超时 |
| `GRAPHIFY_BIN` | `graphify` | graphify 可执行文件 |
| `NOEMA_INSTALL_GRAPHIFY` | `true` | 创建内容库时是否执行上游安装器 |

OpenCode session 当前显式允许全部权限，但关闭交互式 `question` 工具（服务没有用户回答回路）。内容库的工作目录、摄入暂存目录和服务侧提交校验仍由 Noema 管理；这不是面向不受信任租户的主机级沙箱。

## HTTP API

健康检查：

```bash
curl http://127.0.0.1:8787/v1/health
```

创建内容库：

```bash
curl -X POST http://127.0.0.1:8787/v1/libraries \
  -H 'content-type: application/json' \
  -d '{"name":"产品知识库","description":"产品设计和使用文档"}'
```

提交 UTF-8 Markdown/TXT 文档：

```bash
curl -X POST http://127.0.0.1:8787/v1/libraries/<library_id>/documents \
  -H 'content-type: application/json' \
  -d '{"filename":"session-context.md","content":"# Session Context\n\n文档内容"}'
```

摄入是异步任务，可通过下面的接口轮询：

```text
GET /v1/libraries/{library_id}/jobs/{job_id}
```

自然语言查询：

```bash
curl -X POST http://127.0.0.1:8787/v1/libraries/<library_id>/query \
  -H 'content-type: application/json' \
  -d '{"prompt":"解释 Session Context 的主要设计和证据来源"}'
```

## MCP

对外 MCP 端点是标准 Streamable HTTP：

```text
POST /mcp
```

首期工具：

- `kb_ingest_document`
- `kb_query`
- `kb_job_status`
- `kb_health`

MCP 工具都显式接收 `library_id`（健康检查除外）。Noema 不提供 stdio MCP 入口，也不会把自定义知识库 MCP 工具注入 OpenCode；OpenCode 直接使用当前内容库工作区和已安装 Skill。

## 测试

离线端到端测试（`tests/e2e.rs`、`tests/service.rs`）通过假 OpenCode runtime 覆盖 HTTP API、Streamable HTTP MCP（使用 rmcp 官方客户端）、内容库隔离、SHA-256 去重、摄入失败/校验失败的 staging 保留、查询审计和错误路径，不需要模型、网络或 graphify：

```bash
cargo test
```

真实端到端测试（`tests/e2e_live.rs`）默认跳过；显式启用后会连接真实 OpenCode Server，执行 graphify 安装、完整摄入建图、知识节点校验和两次新 session 查询：

```bash
NOEMA_LIVE_E2E=1 OPENCODE_URL=http://127.0.0.1:4096 \
OPENCODE_TEST_MODEL=opencode/deepseek-v4-flash-free \
no_proxy=127.0.0.1,localhost \
cargo test --test e2e_live -- --nocapture
```

服务端可以流式打印 OpenCode 会话的中间过程（text / thinking / tool / skill 调用与结果、step 统计），仅用于服务端日志，HTTP 与 MCP 接口始终只返回最终文本回答。设置 `NOEMA_TRANSCRIPT=1` 启用（终端下自动带颜色，遵循 `NO_COLOR`）：

```bash
NOEMA_TRANSCRIPT=1 noema
```

## 内容库与 Skill

创建内容库时，Noema 会在该内容库项目中直接运行上游 graphify 安装器，并写入中文的 `kb-ingest`、`kb-query`、`kb-maintain` 和独立设计的 `knowledge-compiler` Skill。LLM-WIKI 只作为设计参考，不原样复制。

内容库的 graphify 生命周期是：空内容库只安装插件和 Skill；首篇文本文档摄入时由 OpenCode 执行完整的 `/graphify .`；已有图谱后，新文档或变更文档摄入时执行 `/graphify . --update` 增量更新。

数据默认位于：

```text
data/
├── control.sqlite
└── libraries/{library_id}/
    ├── .opencode/
    ├── raw/
    ├── wiki/
    ├── graph/
    ├── index/
    ├── reviews/
    ├── staging/
    ├── graphify-out/
    ├── purpose.md
    ├── schema.md
    ├── .graphifyignore    # 上游 graphify 的输入边界：只包含 raw/ 和 wiki/
    ├── index.md
    ├── manifest.json
    └── library.sqlite
```

`raw/` 中只接收单文件名的 `.md` 和 `.txt` 文档，原文按 SHA-256 去重并保持不变。摄入 Agent 在 `staging/{job_id}` 中工作，成功后服务才提交允许的知识产物。
