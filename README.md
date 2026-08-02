# Noema

Noema 是由 OpenCode 驱动的文本知识库服务。服务负责内容库、原文、任务和对外协议；OpenCode 负责读取工作区、调用 graphify、编译知识节点并回答查询。

每个内容库都有独立的 OpenCode project、原文、Wiki 节点、图谱产物、索引和 SQLite 文件。查询只接受自然语言 prompt，每次查询都会创建一个新的 OpenCode session。

## 架构

### 系统总览

```mermaid
flowchart TB
    HC["HTTP 客户端<br/>/v1/health · /libraries · /documents · /jobs · /query · export/import"]
    MC["MCP 客户端<br/>Streamable HTTP · /mcp"]
    CLI["noema-cli 命令行客户端<br/>create · list · export · import ·<br/>submit · query · job"]

    subgraph SVC["Noema 服务进程"]
        API["axum HTTP + rmcp MCP 协议层"]
        APP["AppService<br/>内容库 / 异步摄入任务 / 查询编排"]
        RT["OpenCodeAgent 运行时<br/>每次请求创建全新 session"]
        STO["Storage<br/>控制库 + 内容库目录"]
        TR["Transcript<br/>NOEMA_TRANSCRIPT 服务端日志"]
    end

    subgraph DATA["数据目录 data/"]
        CTL[("control.sqlite<br/>libraries · jobs · query_runs")]
        subgraph LIB["单个内容库 libraries/library_id/ — 库间完全隔离"]
            OC[".opencode/<br/>kb-ingest / kb-query / kb-maintain /<br/>knowledge-compiler + graphify 插件"]
            RAW["raw/ 原文<br/>SHA-256 去重 · 只读"]
            WIKI["wiki/ 知识节点<br/>LLM-WIKI 契约 · 9 键 frontmatter"]
            REV["reviews/ 未决声明"]
            GOUT["graphify-out/<br/>graph.json · 报告 · HTML"]
            STG["staging/job_id/<br/>摄入暂存 · 校验后才提交"]
            LDB[("library.sqlite<br/>documents · nodes · FTS")]
        end
    end

    subgraph OCD["OpenCode Server 子进程<br/>由 noema 拉起，随服务停止"]
        SES["Agent session<br/>读取工作区 · 执行 skill"]
    end
    GCLI["graphify CLI<br/>/graphify . 建图"]

    HC --> API
    MC --> API
    API --> APP
    APP --> RT
    APP --> STO
    STO --> CTL
    STO --> LDB
    RT <-->|"SSE 事件流 / 最终文本"| SES
    SES -->|"在库工作目录内读写"| STG
    STG -->|"validate → promote"| WIKI
    STG -->|"validate → promote"| GOUT
    SES -->|"skill 调用"| GCLI
    GCLI --> GOUT
    RT -.->|"text / thinking / tool / skill"| TR
    SVC -.->|"拉起 / 等待就绪 / 随服务停止"| OCD
    CLI -->|"HTTP API（可与服务不在同一台机器）"| API
```

三条进程边界：Noema 服务、OpenCode Server、graphify CLI。OpenCode Server 是 noema 拉起的子进程（启动时等待就绪、Ctrl-C 时一并停止）。Noema 管理内容库、任务与对外协议，并校验摄入产物后才提交；OpenCode Agent 只在一个内容库的工作目录内读写；graphify 产物按库生成。所有管理操作（添加文档、导入导出、查询）都经 HTTP API（`/v1/...`）进行，命令行客户端 `noema-cli` 是对它的封装，允许在与服务不同的机器上操作；快照是单个内容库的完整副本，库与库之间不共享任何文件。

### 摄入流水线

```mermaid
flowchart TB
    REQ["POST /v1/libraries/{id}/documents<br/>filename · title? · content"] --> STORE["store_document<br/>写入 raw/ · SHA-256 去重 ·<br/>登记 library.sqlite · 同步 manifest.json"]
    STORE --> JOB["create_job → queued"]
    JOB --> DUP{"内容已存在？"}
    DUP -->|"是（重复提交）"| SKIP["job → skipped<br/>同步返回 document_path"]
    DUP -->|"否"| TASK["tokio 异步任务<br/>job → running"]

    subgraph STAGE["staging/{job_id}/ — 库根输入副本（prepare_staging）"]
        AGENT["OpenCode session（工作目录 = staging）<br/>knowledge-compiler Skill 写 wiki 节点 ·<br/>/graphify .（首次）或 /graphify . --update（增量）"]
    end

    TASK --> STAGE

    subgraph GATE["validate_staging — 服务端提交校验"]
        direction TB
        V1["无符号链接（.opencode 运行时树除外）"]
        V2["顶层路径白名单（布局常量单一来源）"]
        V3["受保护路径逐字节不变<br/>.graphifyignore · raw/ · purpose.md · schema.md"]
        V4["wiki/*.md 恰好 9 键 YAML frontmatter"]
        V5["不得含 library.sqlite"]
    end

    STAGE --> GATE
    GATE -->|"全部通过"| PROMOTE["promote_staging<br/>整体替换 wiki/ · reviews/ · graphify-out/<br/>覆盖 index.md · manifest.json"]
    PROMOTE --> REBUILD["rebuild_index<br/>library.sqlite 索引 + 全文检索"]
    REBUILD --> CLEAN["cleanup_staging · job → completed"]
    GATE -->|"任一失败"| FAIL["job → failed · staging 保留备查"]
```

Agent 始终只看到库根输入的副本：首次摄入时 staging 没有 `graphify-out/graph.json`，提示词走完整 `/graphify .`；此后的摄入会把库内已有图谱一并复制进 staging，触发 `--update` 增量建图。失败的任务不删除 staging，错误原因记录在 job 的 error 字段。

### 查询时序

```mermaid
sequenceDiagram
    participant C as 客户端 HTTP / MCP / noema-cli
    participant S as AppService
    participant DB as SQLite 控制库 + 库内库
    participant OC as OpenCode Server · session 工作目录 = 库根
    participant G as graphify CLI

    C->>S: POST /v1/libraries/{id}/query · prompt
    S->>DB: record_query → running
    S->>OC: 新建 session · 允许全部权限 · 禁用 question
    Note over OC: 读 purpose.md / schema.md → index.md<br/>摘要优先：wiki 节点的 RAG Version，<br/>不足时再读完整节点与 raw/ 原文
    opt 关系类问题
        OC->>G: graphify query（只读）
        G-->>OC: 作用域子图
    end
    OC-->>S: SSE 事件流，直至 SessionIdle
    S->>OC: 删除 session · 一次性 · 所有退出路径都清理
    Note over S: 只有 text 增量构成答案；thinking 仅进服务端 transcript；<br/>成对 noema-answer 标记之间为最终答案（缺失则回退整段）
    S->>DB: update_query → completed
    Note over S: extract_references：raw/ 与 wiki/ 引用，<br/>raw 来源映射到同名 wiki 节点（存在时）
    S-->>C: answer · references · tool_events

    alt session 出错 / 超时
        S->>DB: update_query → failed
        S-->>C: 502 · query failed
    end
```

查询是只读编排：服务侧不写知识文件，只在控制库记录查询历史，并从答案文本中回抽引用。OpenCode session 的中间过程（thinking、tool 调用）不进响应体，仅可选地落在服务端 transcript 日志。

### 任务状态机

```mermaid
stateDiagram-v2
    [*] --> queued: create_job
    queued --> skipped: 文档 SHA-256 重复
    queued --> running: 异步摄入任务开始
    running --> completed: 校验 → 提交 → 重建索引
    running --> failed: OpenCode / 校验 / 提交任一环节出错
    skipped --> [*]
    completed --> [*]
    failed --> [*]
```

查询历史 `query_runs` 复用这个状态机的 `running / completed / failed` 子集（没有 queued 与 skipped 阶段）。

### 模块结构

```mermaid
flowchart LR
    MAIN["main<br/>CLI 参数 → Config"] --> SUP["supervisor<br/>拉起 OpenCode 子进程 · 等待就绪 ·<br/>对外服务 · Ctrl-C 一并停止"]
    SUP --> SVC["service::AppService<br/>库管理 · 摄入任务 · 查询编排 · 提示词契约"]

    HTTP["http<br/>axum /v1/* · 快照流式收发"] --> SVC
    MCP["mcp<br/>rmcp Streamable HTTP · /mcp"] --> SVC
    CLI["bin/noema-cli<br/>reqwest HTTP 客户端"] -.->|"HTTP（可远程）"| HTTP

    SVC --> RT["runtime::OpenCodeAgent<br/>session 生命周期 · 事件收集 · 答案契约"]
    SVC --> SNAP["snapshot<br/>导出打包 · 导入校验 / 解包 / 修复"]
    SVC --> BOOT["bootstrap<br/>graphify 安装器 · 四个 Skill"]
    SVC --> REF["references<br/>答案引用抽取"]
    RT -.->|"NOEMA_TRANSCRIPT"| TR["transcript<br/>服务端彩色日志"]

    subgraph STO["storage/ — 持久层"]
        SM["mod<br/>Storage · control.sqlite"]
        SC["control<br/>库 / 任务 / 查询 CRUD · slugify"]
        SD["documents<br/>library.sqlite · raw 写入 · 索引重建"]
        SS["staging<br/>prepare / validate / promote / cleanup"]
        SL["layout<br/>布局常量 · 建库种子模板"]
        SF["fsutil<br/>copy_path · write_atomic"]
    end
    SVC --> SM
```

`storage/` 的五个子模块以 impl 块共享 `mod.rs` 中的 `Storage`；布局常量（建库、暂存输入、提交白名单、受保护路径）只定义在 `layout` 一处。`error` / `models` / `config` 横切所有模块，图中未画。

### 快照导出 / 导入

```mermaid
flowchart LR
    subgraph EXP["导出 GET /v1/libraries/{id}/export"]
        direction TB
        E1["写入 noema-snapshot.json 清单<br/>format v1 · 名称 · 来源库 id"]
        E2["遍历库根目录"]
        E3["排除 staging/ · library.sqlite-* 边车 ·<br/>.opencode/node_modules · .opencode/opencode-loop ·<br/>所有符号链接"]
        E4["gzip tar 流式返回"]
        E1 --> E2 --> E3 --> E4
    end

    subgraph IMP["导入 POST /v1/libraries/import"]
        direction TB
        I1["解包到 data/jobs/import-* 临时目录<br/>拒绝绝对路径 / .. / 符号链接 / 硬链接（400）"]
        I2["校验清单格式与版本<br/>create_library：新 id · 新目录 · 新控制记录"]
        I3["覆盖拷贝 → rebuild_index 重建全部派生产物"]
        I4{"归档含 .opencode/？"}
        I5["graphify 安装器补齐"]
        I6["刷新四个 Noema Skill → 201 新库 JSON"]
        I1 --> I2 --> I3 --> I4
        I4 -->|"否"| I5 --> I6
        I4 -->|"是"| I6
    end

    E4 -.->|"归档可分发到其他机器 / 其他用户"| I1
```

`library.sqlite` 本身随快照导出（只存库内相对路径，可移植），排除的只是进程边车文件。导入始终新建内容库：名称可改可重名（知识身份由节点 `node_id` 承载），任一步失败都完整回滚（删新库目录、撤控制记录），临时目录无论如何都会清理。

## 依赖

- Rust stable
- `opencode` 可执行文件（取 PATH 上的 `opencode`）：noema 自行拉起并管理 OpenCode Server 子进程
- `graphify` CLI。创建内容库时执行 `graphify install --platform opencode --project`
- 本地 OpenCode SDK：`/mnt/data/code/agentic_auxilary/crates/services/opencode-rs`

## 启动

noema 自行拉起并管理 OpenCode Server 子进程：绑定 127.0.0.1 并自动选取空闲端口，启动时等待其就绪再对外服务，Ctrl-C 时随服务一并停止。实际地址见 `/v1/health` 返回的 `opencode_url`。

```bash
NOEMA_DATA_DIR=data cargo run
```

模型默认用内置的 `opencode/deepseek-v4-flash-free`，可用 `--model` 或 `OPENCODE_MODEL` 覆盖。

所有配置项同样可以用命令行参数直接传入（环境变量是回退项），完整列表见 `cargo run -- --help`。

常用配置：

| 环境变量 | 默认值 | 作用 |
| --- | --- | --- |
| `NOEMA_BIND` | `127.0.0.1:8787` | Noema 监听地址 |
| `NOEMA_DATA_DIR` | `data` | 服务数据目录 |
| `OPENCODE_MODEL` | `opencode/deepseek-v4-flash-free` | 模型标识 |
| `OPENCODE_TIMEOUT_SECS` | `1800` | 单个 Agent session 超时 |

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

导出内容库快照（返回 gzip tar 归档；路径参数接受内容库 id，或唯一的名称）：

```bash
curl -o base-regulations.tar.gz \
  http://127.0.0.1:8787/v1/libraries/<library_id>/export
```

导入快照（请求体即归档；始终创建一个全新的内容库，成功返回 201 和新库 JSON；`name`/`description` 可选，缺省取快照记录的元数据）：

```bash
curl -X POST --data-binary @base-regulations.tar.gz \
  'http://127.0.0.1:8787/v1/libraries/import?name=用户A法规库'
```

快照语义见命令行客户端一节：快照是单个内容库的完整副本（含 graphify 产物、wiki 节点和 library.sqlite），导入始终新建内容库，失败完整回滚，含路径穿越、符号链接或硬链接的归档一律拒绝（400）。

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

## 命令行客户端（noema-cli）

`noema-cli` 是 HTTP API 的命令行封装——所有操作都经过运行中的 `noema` 服务，可以和服务不在同一台机器（地址用 `--server` 或 `$NOEMA_SERVER` 指定，默认 `http://127.0.0.1:8787`）：

```bash
noema-cli status
noema-cli create 基础法规库
noema-cli list
noema-cli export 基础法规库 -o base-regulations.tar.gz
noema-cli import base-regulations.tar.gz --name 用户A法规库
noema-cli submit 用户A法规库 ./my-regulation.md
noema-cli job 用户A法规库 <job_id>
noema-cli query 用户A法规库 "第一条讲了什么？"
```

导出把服务端生成的快照下载到本地文件，导入把本地归档上传给服务端；提交文档时读取本地 `.md`/`.txt` 并上传内容。导入语义：始终新建内容库，失败完整回滚，含路径穿越或符号/硬链接的归档一律拒绝（服务端返回 400，客户端非零退出并打印错误）。

快照是一个内容库的完整副本（gzip tar）：`raw/` 原文、`wiki/` 知识节点（LLM-WIKI 编译产物）、`reviews/`、`graphify-out/` 图谱产物、`.opencode/` Skill 与插件、`library.sqlite`（去重与索引记录）。归档不含 `staging/`、运行时状态（`node_modules`、会话记录、SQLite sidecar）和任何符号链接。

导入始终创建一个全新的内容库：新 id、新目录、独立数据库，失败时完整回滚。不同内容库完全隔离——不共享任何文件、不互相引用，跨库复用只通过"导出 → 导入"的副本发生。内容库名称只是别名，导入时可以随意重命名、可以重名；知识的身份由节点内的 `node_id` 承载。若快照缺少 `.opencode/`，导入会执行 graphify 安装器补齐（需要 graphify CLI）。

典型复用流程：团队维护一份基础法规库，导出快照后分发给各用户；每个用户导入到自己的数据目录，再通过正常摄入流程增量添加自己的法规（摄入 Agent 执行 graphify `--update` 增量建图）。

## 测试

`tests/service.rs` 和库内单元测试通过假 OpenCode runtime 覆盖服务层（摄入、查询、内容库隔离、SHA-256 去重、graphify 增量建图提示词）、HTTP 与 Streamable HTTP MCP 挂载和快照导入导出；`tests/cli.rs` 拉起真实 `noema` 及其 OpenCode Server 子进程，验证 noema-cli 经 HTTP 的导出→导入往返和恶意归档拒绝。建库会真实执行 graphify 安装器（离线可用）；不需要模型或网络，但 PATH 上需要 `opencode` 与 `graphify`：

```bash
cargo test
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
