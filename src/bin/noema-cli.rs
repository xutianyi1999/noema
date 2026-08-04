//! noema-cli: command-line client for the Noema service.
//!
//! A thin wrapper over the HTTP JSON API — every operation travels the
//! network to a running `noema` server, so the client works from any
//! machine: documents are read locally and uploaded, snapshots are
//! downloaded to / uploaded from local files, queries print the server's
//! answer. This binary never touches the server's data directory.
//!
//! Output styling uses the same pair as the server transcript: anstyle
//! styles embedded through anstream, which strips them when stdout is not
//! a terminal (NO_COLOR / FORCE_COLOR / CLICOLOR honoured); tables come
//! from comfy-table, which applies its own styling only on a terminal.

use std::{io::Write as _, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use comfy_table::{Attribute, Cell, Color, Table, presets::UTF8_FULL};
use noema::style::{BOLD, DIM, GREEN, RED, YELLOW, paint, stderr, stdout};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

/// Noema 命令行客户端：经 HTTP API 管理内容库（可与服务不在同一台机器）。需要先启动 noema 服务。
#[derive(Parser)]
#[command(name = "noema-cli", version, about)]
struct Cli {
    /// noema 服务地址
    #[arg(
        long,
        global = true,
        env = "NOEMA_SERVER",
        default_value = "http://127.0.0.1:8787"
    )]
    server: String,
    /// 服务鉴权令牌（服务以 --auth-token 启用鉴权时必须；每个请求携带 Authorization: Bearer <token>）
    #[arg(long, global = true, env = "NOEMA_AUTH_TOKEN")]
    auth_token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 服务状态（健康检查 + 数据目录与模型配置）
    Status,
    /// 创建内容库（服务工作目录与 Skill 由服务端生成）
    Create {
        /// 内容库名称（唯一；原样用作内容库 id 与目录名）
        name: String,
        /// 内容库描述
        #[arg(long)]
        description: Option<String>,
    },
    /// 列出所有内容库
    List,
    /// 导出内容库：服务端生成快照，下载到本地文件
    Export {
        /// 内容库 id，或唯一的内容库名称
        library: String,
        /// 本地输出路径（默认 <内容库>.tar.gz）
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// 把本地快照归档上传给服务端，导入为全新的内容库
    Import {
        /// 本地快照归档路径（.tar.gz）
        archive: PathBuf,
        /// 新内容库名称（默认用快照记录的原名；与已有库重名会被拒绝）
        #[arg(long)]
        name: Option<String>,
        /// 新内容库描述
        #[arg(long)]
        description: Option<String>,
    },
    /// 提交本地 .md/.txt 文档到内容库，触发异步摄入（多个文档合并为一次摄入任务）
    Submit {
        /// 内容库 id，或唯一的内容库名称
        library: String,
        /// 本地文档路径（.md/.txt，单级文件名；可传多个）
        #[arg(value_name = "FILE", num_args = 1..)]
        files: Vec<PathBuf>,
        /// 文档标题（仅单文件提交可用）
        #[arg(long)]
        title: Option<String>,
    },
    /// 查询摄入任务状态
    Job {
        /// 内容库 id，或唯一的内容库名称
        library: String,
        /// 任务 id
        job_id: String,
    },
    /// 自然语言查询（默认创建新 Agent session；可复用返回的会话）
    Query {
        /// 内容库 id，或唯一的内容库名称
        library: String,
        /// 查询提示词
        prompt: String,
        /// 复用此前查询返回的会话 id
        #[arg(long)]
        session_id: Option<String>,
    },
}

type BoxError = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(stderr(), "{}", paint(RED, &format!("noema-cli: {error}")));
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), BoxError> {
    // Deadlines so a wedged server cannot hang the CLI forever: a short
    // connect timeout, plus a generous overall deadline that still covers a
    // synchronous query running a full agent session server-side.
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(2 * 60 * 60));
    if let Some(token) = &cli.auth_token {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}").parse()?,
        );
        builder = builder.default_headers(headers);
    }
    let client = builder.build()?;
    let base = cli.server.trim_end_matches('/').to_string();
    match cli.command {
        Command::Status => cmd_status(&client, &base).await,
        Command::Create { name, description } => {
            cmd_create(&client, &base, name, description.as_deref()).await
        }
        Command::List => cmd_list(&client, &base).await,
        Command::Export { library, output } => cmd_export(&client, &base, library, output).await,
        Command::Import {
            archive,
            name,
            description,
        } => {
            cmd_import(
                &client,
                &base,
                archive,
                name.as_deref(),
                description.as_deref(),
            )
            .await
        }
        Command::Submit {
            library,
            files,
            title,
        } => cmd_submit(&client, &base, library, files, title.as_deref()).await,
        Command::Job { library, job_id } => cmd_job(&client, &base, library, job_id).await,
        Command::Query {
            library,
            prompt,
            session_id,
        } => cmd_query(&client, &base, library, prompt, session_id.as_deref()).await,
    }
}

async fn cmd_status(client: &reqwest::Client, base: &str) -> Result<(), BoxError> {
    let response = send(client.get(format!("{base}/v1/health"))).await?;
    let value = response.json::<Value>().await?;
    let healthy = value["status"].as_str() == Some("ok");
    let headline = if healthy {
        paint(GREEN, "● 服务正常")
    } else {
        paint(RED, &format!("● 服务异常（{}）", value["status"]))
    };
    let _ = writeln!(stdout(), "{headline}  {base}");
    print_kv(vec![
        ("数据目录", plain(string_field(&value, "data_dir"))),
        ("OpenCode", plain(string_field(&value, "opencode_url"))),
        ("模型", plain(string_field(&value, "configured_model"))),
    ]);
    Ok(())
}

async fn cmd_create(
    client: &reqwest::Client,
    base: &str,
    name: String,
    description: Option<&str>,
) -> Result<(), BoxError> {
    let response = send(
        client
            .post(format!("{base}/v1/libraries"))
            .json(&json!({ "name": name, "description": description })),
    )
    .await?;
    print_library("✔ 已创建内容库", &response.json::<Value>().await?);
    Ok(())
}

async fn cmd_list(client: &reqwest::Client, base: &str) -> Result<(), BoxError> {
    let response = send(client.get(format!("{base}/v1/libraries"))).await?;
    let libraries = response.json::<Vec<Value>>().await?;
    if libraries.is_empty() {
        let _ = writeln!(stdout(), "{}  server={base}", paint(DIM, "（暂无内容库）"));
        return Ok(());
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_header(vec![
        Cell::new("ID").add_attribute(Attribute::Bold),
        Cell::new("名称").add_attribute(Attribute::Bold),
        Cell::new("路径").add_attribute(Attribute::Bold),
    ]);
    for library in &libraries {
        table.add_row([
            string_field(library, "id"),
            string_field(library, "name"),
            string_field(library, "root"),
        ]);
    }
    let _ = writeln!(stdout(), "{table}");
    Ok(())
}

async fn cmd_export(
    client: &reqwest::Client,
    base: &str,
    library: String,
    output: Option<PathBuf>,
) -> Result<(), BoxError> {
    let url = format!("{base}/v1/libraries/{}/export", encode(&library));
    let mut response = send(client.get(url)).await?;
    let output = output
        .unwrap_or_else(|| PathBuf::from(format!("{}.tar.gz", library.replace(['/', '\\'], "-"))));
    // Download into a sibling `.part` file and rename it into place only
    // once the whole archive has arrived: a download that fails mid-stream
    // must not leave a truncated archive at the destination nor destroy
    // whatever was there before.
    let mut part = output.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let mut file = tokio::fs::File::create(&part).await?;
    let mut bytes = 0u64;
    let copied = copy_chunks(&mut response, &mut file, &mut bytes).await;
    drop(file);
    if let Err(error) = copied {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(error);
    }
    tokio::fs::rename(&part, &output).await?;
    let _ = writeln!(
        stdout(),
        "{} {library} → {}（{bytes} 字节）",
        paint(GREEN, "✔ 已导出"),
        output.display()
    );
    Ok(())
}

async fn copy_chunks(
    response: &mut reqwest::Response,
    file: &mut tokio::fs::File,
    bytes: &mut u64,
) -> Result<(), BoxError> {
    while let Some(chunk) = response.chunk().await? {
        *bytes += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

async fn cmd_import(
    client: &reqwest::Client,
    base: &str,
    archive: PathBuf,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<(), BoxError> {
    // Stream the archive from disk: snapshots can approach the server's
    // 512 MiB upload cap, and buffering the whole file would needlessly
    // double the CLI's memory footprint.
    let file = tokio::fs::File::open(&archive)
        .await
        .map_err(|error| format!("cannot read {}: {error}", archive.display()))?;
    let mut query: Vec<(&str, &str)> = Vec::new();
    if let Some(name) = name {
        query.push(("name", name));
    }
    if let Some(description) = description {
        query.push(("description", description));
    }
    let response = send(
        client
            .post(format!("{base}/v1/libraries/import"))
            .query(&query)
            .header(reqwest::header::CONTENT_TYPE, "application/gzip")
            .body(reqwest::Body::wrap_stream(
                tokio_util::io::ReaderStream::new(file),
            )),
    )
    .await?;
    print_library("✔ 已导入为全新内容库", &response.json::<Value>().await?);
    Ok(())
}

async fn cmd_submit(
    client: &reqwest::Client,
    base: &str,
    library: String,
    files: Vec<PathBuf>,
    title: Option<&str>,
) -> Result<(), BoxError> {
    if files.len() > 1 && title.is_some() {
        return Err("--title 只支持单个文件提交".into());
    }
    let mut documents = Vec::with_capacity(files.len());
    for file in &files {
        let filename = file
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| format!("invalid file name: {}", file.display()))?
            .to_string();
        let content = tokio::fs::read_to_string(file)
            .await
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        documents.push(json!({ "filename": filename, "content": content, "title": title }));
    }
    // One submission route for one file or many; every response entry names
    // its stored path and whether the ingest job covers it.
    let response = send(
        client
            .post(format!(
                "{base}/v1/libraries/{}/documents",
                encode(&library)
            ))
            .json(&json!({ "documents": documents })),
    )
    .await?;
    let value = response.json::<Value>().await?;
    let entries = value["documents"].as_array().cloned().unwrap_or_default();
    if let [entry] = entries.as_slice() {
        let path = string_field(entry, "document_path");
        let headline = if entry["skipped"].as_bool().unwrap_or(false) {
            paint(YELLOW, "● 内容重复（SHA-256）——已登记，未触发摄入")
        } else {
            paint(GREEN, "✔ 已提交，摄入任务已创建")
        };
        let _ = writeln!(stdout(), "{headline}  {path}");
    } else {
        let _ = writeln!(
            stdout(),
            "{}",
            paint(
                GREEN,
                &format!("✔ 已提交 {} 个文档，合并为一次摄入任务", entries.len())
            )
        );
        for entry in &entries {
            let path = string_field(entry, "document_path");
            let line = if entry["skipped"].as_bool().unwrap_or(false) {
                paint(YELLOW, "● 内容重复（SHA-256）——已登记，未触发摄入")
            } else {
                paint(GREEN, "✔ 已提交")
            };
            let _ = writeln!(stdout(), "  {line}  {path}");
        }
    }
    let job_id = string_field(&value, "job_id");
    print_kv(vec![
        ("任务", plain(job_id.clone())),
        (
            "查看进度",
            plain(format!("noema-cli job {library} {job_id}")),
        ),
    ]);
    Ok(())
}

async fn cmd_job(
    client: &reqwest::Client,
    base: &str,
    library: String,
    job_id: String,
) -> Result<(), BoxError> {
    let response = send(client.get(format!(
        "{base}/v1/libraries/{}/jobs/{}",
        encode(&library),
        encode(&job_id)
    )))
    .await?;
    let value = response.json::<Value>().await?;
    let status = string_field(&value, "status");
    let status_cell = match status.as_str() {
        "completed" => colored(Color::Green, status),
        "failed" => colored(Color::Red, status),
        "skipped" => dim(status),
        _ => colored(Color::Yellow, status),
    };
    let error = value["error"].as_str();
    let session = value["session_id"].as_str();
    let mut rows: Vec<(&str, Cell)> = vec![
        ("任务", plain(string_field(&value, "job_id"))),
        ("内容库", plain(string_field(&value, "library_id"))),
        ("类型", plain(string_field(&value, "kind"))),
        ("状态", status_cell),
    ];
    if let Some(error) = error {
        rows.push(("错误", colored(Color::Red, error.to_string())));
    }
    rows.push(("会话", optional(session)));
    rows.push(("创建时间", plain(string_field(&value, "created_at"))));
    rows.push(("更新时间", plain(string_field(&value, "updated_at"))));
    print_kv(rows);
    Ok(())
}

async fn cmd_query(
    client: &reqwest::Client,
    base: &str,
    library: String,
    prompt: String,
    session_id: Option<&str>,
) -> Result<(), BoxError> {
    let mut body = json!({ "prompt": prompt });
    if let Some(session_id) = session_id {
        body["session_id"] = Value::String(session_id.to_string());
    }
    let response = send(
        client
            .post(format!("{base}/v1/libraries/{}/query", encode(&library)))
            .json(&body),
    )
    .await?;
    let value = response.json::<Value>().await?;
    // The answer is Markdown: termimad renders it for the terminal (styled
    // on a tty, plain structured text when piped).
    termimad::print_text(&string_field(&value, "answer"));
    let references = value["references"].as_array();
    if references.is_some_and(|items| !items.is_empty()) {
        let _ = writeln!(stdout());
        let _ = writeln!(stdout(), "{}", paint(BOLD, "来源"));
        for item in references.unwrap() {
            let title = string_field(item, "title");
            let source = string_field(item, "source");
            // `  · [2] 中华人民共和国担保法  第十八条  raw/担保法.md#char=812,848 → wiki/连带责任保证.md`
            let mut line = format!("  · [{}] {title}", item["id"].as_u64().unwrap_or(0));
            if let Some(locator) = item["locator"].as_str() {
                line.push_str(&format!("  {locator}"));
            }
            line.push_str(&format!("  {source}"));
            if let (Some(start), Some(end)) = (item["start"].as_u64(), item["end"].as_u64()) {
                line.push_str(&format!("#char={start},{end}"));
            }
            if let Some(node) = item["node"].as_str() {
                line.push_str(&format!(" → {node}"));
            }
            let _ = writeln!(stdout(), "{}", paint(DIM, &line));
        }
    }
    // Always surface the id so a user can copy it into `--session-id` for a
    // follow-up. The HTTP and MCP responses already include this field.
    let _ = writeln!(
        stdout(),
        "{} {}",
        paint(DIM, "会话:"),
        string_field(&value, "session_id")
    );
    Ok(())
}

/// Send the request, turning any non-success status into an error. The
/// server reports failures as JSON bodies shaped like `{"error": "..."}`.
async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, BoxError> {
    let response = request.send().await?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|item| item.as_str())
                .map(str::to_string)
        })
        .unwrap_or(body);
    Err(format!("server returned {status}: {message}").into())
}

/// Percent-encode one URL path segment (library ids/names, job ids).
fn encode(segment: &str) -> String {
    utf8_percent_encode(segment, NON_ALPHANUMERIC).to_string()
}

/// One content library as a styled headline plus a key/value block.
fn print_library(headline: &str, library: &Value) {
    let _ = writeln!(stdout(), "{}", paint(GREEN, headline));
    print_kv(vec![
        ("ID", plain(string_field(library, "id"))),
        ("名称", plain(string_field(library, "name"))),
        ("路径", plain(string_field(library, "root"))),
    ]);
}

/// Borderless key/value block; comfy-table aligns the columns and handles
/// CJK widths, its styling only applies on a terminal.
fn print_kv(rows: Vec<(&str, Cell)>) {
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::NOTHING);
    for (key, value) in rows {
        table.add_row([Cell::new(key).add_attribute(Attribute::Dim), value]);
    }
    let _ = writeln!(stdout(), "{table}");
}

fn string_field(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or("—").to_string()
}

fn plain(text: String) -> Cell {
    Cell::new(text)
}

fn dim(text: String) -> Cell {
    Cell::new(text).add_attribute(Attribute::Dim)
}

fn optional(text: Option<&str>) -> Cell {
    match text {
        Some(text) => Cell::new(text.to_string()),
        None => dim("—".to_string()),
    }
}

fn colored(color: Color, text: String) -> Cell {
    Cell::new(text).fg(color)
}
