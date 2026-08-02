# Noema

Noema 是由 OpenCode 驱动的文本知识库服务。服务负责内容库、原文、任务和对外协议；OpenCode 负责读取工作区、调用 graphify、编译知识节点并回答查询。

每个内容库都有独立的 OpenCode project、原文、Wiki 节点、图谱产物、索引和 SQLite 文件。查询只接受自然语言 prompt，每次查询都会创建一个新的 OpenCode session。

## 架构

### 设计原则

- **一库一世界**：每个内容库拥有独立的整套文件、独立的数据库和独立的 OpenCode 项目；库与库不共享文件、不互相引用，跨库的唯一途径是"导出 → 导入"副本。
- **三方分工**：Noema 管边界与规则（协议、任务、校验、提交、进出库）；OpenCode Agent 负责知识生产（编译节点、建图、回答问题）；graphify 作为 Agent 的技能提供图谱构建与查询。
- **先校验后入库**：Agent 只在暂存副本上工作，知识产物经服务端校验后才进入正式内容库；失败时正式库不受任何影响。
- **唯一对外入口**：所有管理能力都经 HTTP API；MCP 与 noema-cli 是同一套能力的两种形态，命令行可以远程操作服务。
- **证据可追溯**：知识节点声明来源，回答携带引用，服务端把引用映射回知识节点；任务与查询在控制面留下可审计历史。

### 分层与依赖层级

```mermaid
flowchart TB
    subgraph L1["客户端层"]
        HC["HTTP 客户端"]
        MC["MCP 客户端（AI 工具）"]
        CLI["noema-cli 命令行（可远程）"]
    end

    subgraph L2["协议接入层 — 唯一对外入口"]
        HTTP["HTTP API /v1/*"]
        MCPE["MCP Streamable HTTP · /mcp"]
    end

    subgraph L3["业务编排层 — Noema 服务进程"]
        direction LR
        LIB["内容库管理"]
        ING["摄入任务编排"]
        QRY["查询编排"]
        SNAP["快照进出库"]
    end

    subgraph L4["Agent 运行时层"]
        RT["每次请求一个一次性 OpenCode session<br/>收集答案与中间过程"]
    end

    subgraph L5["数据层 — 目录与文件结构见下节"]
        CTL[("control.sqlite<br/>内容库注册 · 摄入任务 · 查询历史")]
        FS[("libraries/{library_id}/<br/>每库一套完整文件与库内数据库")]
    end

    subgraph L6["受管外部进程"]
        OC["OpenCode Server — 服务拉起的子进程 · 随服务停止"]
        GF["graphify — 由 OpenCode 以 skill 调用"]
    end

    L1 --> L2 --> L3
    LIB --> CTL
    LIB --> FS
    ING --> RT
    QRY --> RT
    ING --> FS
    SNAP --> FS
    RT <-->|"session 与事件流"| OC
    OC -.->|"skill 调用"| GF
    OC -->|"只在被服务的内容库内读写"| FS
    GF -->|"产物落在被服务的内容库内"| FS
```

依赖关系单向向下：客户端只与协议层交互，协议层只做协议翻译，业务规则只存在于业务编排层。三个进程协作——Noema 服务负责编排与校验；OpenCode Server 是服务拉起并管理的子进程（启动等待就绪、Ctrl-C 一并停止），且只在被服务的内容库目录内读写；graphify 平时由 OpenCode 以 skill 调用，建库与快照导入时由 Noema 运行其安装器。

### 内容库目录与文件结构

```text
data/                                  数据根目录（NOEMA_DATA_DIR）
├── control.sqlite                     控制面：内容库注册、摄入任务、查询历史
├── jobs/                              快照导入临时目录（导入结束必定清理）
└── libraries/{library_id}/            一个内容库 —— 与其他库完全隔离
    ├── purpose.md                     内容库定位：范围、关键问题、术语、更新政策
    ├── schema.md                      知识节点契约的人类可读声明
    ├── index.md                       全库知识索引（派生，提交后重建）
    ├── manifest.json                  原文清单（派生）
    ├── .graphifyignore                graphify 输入边界：只含 raw/ 与 wiki/
    ├── AGENTS.md                      Agent 行为说明（graphify 安装器写入）
    ├── library.sqlite                 库内数据库：原文去重、节点注册、全文检索
    ├── .opencode/                     OpenCode 项目：四个 Noema Skill、graphify 插件与配置
    ├── raw/                           原文：.md/.txt、单级文件名、SHA-256 去重、入库后只读
    ├── wiki/                          知识节点：LLM-WIKI 契约、9 键 frontmatter
    ├── reviews/                       未解决的声明与低置信度关系
    ├── graphify-out/                  图谱产物：graph.json、报告、交互 HTML、增量缓存
    └── staging/{job_id}/              摄入工作区：库根输入的副本，校验通过才提交
```

读写职责与边界：

| 目录 / 文件 | 业务角色 | 写入方 | 边界约束 |
| --- | --- | --- | --- |
| `raw/` | 事实来源，一切知识的证据基础 | Noema（提交文档） | SHA-256 去重、写入后只读；Agent 不得修改 |
| `wiki/` | 编译后的知识节点 | Agent（经 staging 提交） | frontmatter 恰好 9 键；正文含定义 / 证据 / 示例 / 局限 / RAG 压缩摘要 / 引用 |
| `reviews/` | 未解决的冲突与低置信度结论 | Agent（经 staging 提交） | 与正式知识分离，留待人工或后续任务处理 |
| `graphify-out/` | 知识图谱、报告与增量缓存 | graphify（经 staging 提交） | 输入被限定为 `raw/` + `wiki/`；与 raw/wiki 同级，只服务图谱查询 |
| `index.md` · `manifest.json` · `library.sqlite` | 派生索引与去重记录 | Noema 自动重建 | 不手写；永远由原文、知识节点和库内数据库再生 |
| `staging/{job_id}/` | 摄入隔离工作区 | Noema 创建与清理 | 成功则提交允许的知识产物并清理；失败则保留备查，库不受影响 |
| `purpose.md` · `schema.md` · `.graphifyignore` | 内容库契约与边界 | 建库时种入 | 摄入校验要求逐字节未变；Agent 与 graphify 都在其划定的范围内活动 |
| `.opencode/` · `AGENTS.md` | Agent 能力与行为说明 | Noema 与 graphify 安装器 | 建库与快照导入时写入/刷新；不属于知识提交物 |

知识进入正式库的唯一路径：Agent 在 `staging/{job_id}/` 内写入 → Noema 校验 → 只把白名单内的知识产物（`wiki/`、`reviews/`、`graphify-out/`、`index.md`、`manifest.json`）提交到库根。

### 摄入业务流

```mermaid
flowchart TB
    S1["提交文档"] --> S2["去重判定（SHA-256）<br/>新原文写入 raw/ 并登记"]
    S2 --> S3{"内容是否已入库？"}
    S3 -->|"是"| S4["任务记为 skipped"]
    S3 -->|"否"| S5["创建摄入任务<br/>库根输入整套复制进隔离的 staging 工作区"]
    S5 --> S6["OpenCode Agent 在 staging 内工作：<br/>按节点契约编译知识节点 ·<br/>首次完整建图，此后增量更新图谱"]
    S6 --> S7{"服务端提交校验<br/>工作区边界干净 · 受保护文件逐字节未变 ·<br/>节点契约完整 · 无禁入文件"}
    S7 -->|"通过"| S8["仅允许的知识产物提交回库根"]
    S8 --> S9["重建库内索引与全文检索"]
    S9 --> S10["任务完成 · 工作区清理"]
    S7 -->|"失败"| S11["任务失败 · 工作区保留备查 · 正式库不受影响"]
```

提交是"Agent 产出"与"库内知识"的分界线：Agent 完成前正式库一个字节不变，校验失败后同样不变。失败的工作区留在磁盘上，配合任务的错误信息可反复排查。

### 查询业务流

```mermaid
sequenceDiagram
    participant U as 提问方 HTTP / MCP / CLI
    participant N as Noema
    participant A as OpenCode Agent · 工作在内容库根目录
    participant G as graphify

    U->>N: 自然语言问题
    N->>A: 创建一次性 session
    Note over A: 先读内容库契约，再读索引；<br/>摘要优先 —— 先读相关知识节点的 RAG 压缩摘要，<br/>不足时再读完整节点与 raw/ 原文
    opt 涉及关系的问题
        A->>G: 图谱查询（只读）
        G-->>A: 作用域子图
    end
    A-->>N: 带相对路径引用的证据化答案
    N->>N: 规整答案格式 · 引用映射回知识节点
    N-->>U: 答案 · 被引来源 · 对应知识节点
```

查询全程不写任何知识文件；session 用完即销毁，只在控制面留下查询历史。引用指向原文时，服务端同时附上对应的知识节点（若存在），形成"原文 → 节点"两级追溯。

### 摄入任务与查询状态

```mermaid
stateDiagram-v2
    [*] --> queued: 任务创建
    queued --> skipped: 文档内容重复
    queued --> running: 开始处理
    running --> completed: 编译 → 校验 → 提交 → 重建索引
    running --> failed: 任一环节出错
    skipped --> [*]
    completed --> [*]
    failed --> [*]
```

查询历史复用其中 `running / completed / failed` 子集。任务错误信息、以及失败时保留的 staging 工作区，是排障依据。

### 内容库隔离与快照复用

```mermaid
flowchart LR
    BASE["基础内容库<br/>共享文档 + 编译好的知识 + 图谱"] -->|"导出快照：单库完整副本"| FILE[("快照归档<br/>gzip tar")]
    FILE -->|"分发（可离线）"| IMpa["用户 A 导入"]
    FILE --> IMpb["用户 B 导入"]
    IMpa --> NEWA["全新内容库 A<br/>新 id · 新目录 · 独立数据库"]
    IMpb --> NEWB["全新内容库 B<br/>新 id · 新目录 · 独立数据库"]
    NEWA --> ADDA["正常摄入流程添加自有文档<br/>图谱自动增量更新"]
    NEWB --> ADDB["正常摄入流程添加自有文档<br/>图谱自动增量更新"]
```

隔离与复用规则：

- 库是完全隔离的单位：不共享文件、不互引；跨库只有"导出 → 导入"副本这一条路。
- 快照是单个内容库的完整副本（含图谱产物与库内数据库，不含 staging 工作区与运行时状态）；含路径穿越、符号链接或硬链接的归档一律拒绝。
- 导入始终新建内容库，失败完整回滚；名称只是别名，可改可重名，知识的身份由节点内 `node_id` 承载。

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
| `NOEMA_TRANSCRIPT` | `false` | 流式打印会话中间过程（仅服务端日志，等价 `--transcript`） |

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

所有 `/v1/libraries/{library_id}/…` 路由的 `{library_id}` 路径段都接受内容库 id，或唯一的内容库名称；名称重名时返回 400 并要求改用 id。

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

服务端可以流式打印 OpenCode 会话的中间过程（text / thinking / tool / skill 调用与结果、step 统计），仅用于服务端日志，HTTP 与 MCP 接口始终只返回最终文本回答。模型的长段自述与最终答案按部件截取预览（其余以"另有约 N 字未显示"一行带过），工具参数中的路径完整显示。用 `--transcript` 标志启用（环境变量 `NOEMA_TRANSCRIPT` 是回退项；终端下自动带颜色，遵循 `NO_COLOR`）：

```bash
noema --transcript
```

## 内容库与 Skill

创建内容库时，Noema 会在该内容库项目中直接运行上游 graphify 安装器，并写入中文的 `kb-ingest`、`kb-query`、`kb-maintain` 和独立设计的 `knowledge-compiler` Skill。LLM-WIKI 只作为设计参考，不原样复制。

内容库的 graphify 生命周期是：空内容库只安装插件和 Skill；首篇文本文档摄入时由 OpenCode 执行完整的 `/graphify .`；已有图谱后，新文档或变更文档摄入时执行 `/graphify . --update` 增量更新。

目录与文件结构、各路径的读写职责见上文「架构 → 内容库目录与文件结构」。
