//! noema-cli: command-line client for the Noema service.
//!
//! A thin wrapper over the HTTP JSON API — every operation travels the
//! network to a running `noema` server, so the client works from any
//! machine: documents are read locally and uploaded, snapshots are
//! downloaded to / uploaded from local files, queries print the server's
//! answer. This binary never touches the server's data directory.

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 服务健康检查
    Health,
    /// 创建内容库（服务工作目录与 Skill 由服务端生成）
    Create {
        /// 内容库名称（只是别名，可重复）
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
        /// 新内容库名称（默认用快照记录的原名；名称只是别名，可重复）
        #[arg(long)]
        name: Option<String>,
        /// 新内容库描述
        #[arg(long)]
        description: Option<String>,
    },
    /// 提交本地 .md/.txt 文档到内容库，触发异步摄入
    Submit {
        /// 内容库 id，或唯一的内容库名称
        library: String,
        /// 本地文档路径（.md/.txt，单级文件名）
        file: PathBuf,
        /// 文档标题
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
    /// 自然语言查询（服务端创建全新 Agent session 作答）
    Query {
        /// 内容库 id，或唯一的内容库名称
        library: String,
        /// 查询提示词
        prompt: String,
    },
}

type BoxError = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("noema-cli: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), BoxError> {
    let client = reqwest::Client::new();
    let base = cli.server.trim_end_matches('/').to_string();
    match cli.command {
        Command::Health => {
            let response = send(client.get(format!("{base}/v1/health"))).await?;
            let value = response.json::<Value>().await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Command::Create { name, description } => {
            let response = send(
                client
                    .post(format!("{base}/v1/libraries"))
                    .json(&json!({ "name": name, "description": description })),
            )
            .await?;
            print_library_line(&response.json::<Value>().await?);
        }
        Command::List => {
            let response = send(client.get(format!("{base}/v1/libraries"))).await?;
            let libraries = response.json::<Vec<Value>>().await?;
            if libraries.is_empty() {
                println!("(no content libraries) server={base}");
                return Ok(());
            }
            for library in libraries {
                print_library_line(&library);
            }
        }
        Command::Export { library, output } => {
            let url = format!("{base}/v1/libraries/{}/export", encode(&library));
            let mut response = send(client.get(url)).await?;
            let output = output.unwrap_or_else(|| {
                PathBuf::from(format!("{}.tar.gz", library.replace(['/', '\\'], "-")))
            });
            let mut file = tokio::fs::File::create(&output).await?;
            let mut bytes = 0u64;
            while let Some(chunk) = response.chunk().await? {
                bytes += chunk.len() as u64;
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            println!("exported {library} -> {} ({bytes} bytes)", output.display());
        }
        Command::Import {
            archive,
            name,
            description,
        } => {
            let bytes = tokio::fs::read(&archive)
                .await
                .map_err(|error| format!("cannot read {}: {error}", archive.display()))?;
            let mut query: Vec<(&str, String)> = Vec::new();
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
                    .body(bytes),
            )
            .await?;
            print_library_line(&response.json::<Value>().await?);
        }
        Command::Submit {
            library,
            file,
            title,
        } => {
            let filename = file
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| format!("invalid file name: {}", file.display()))?
                .to_string();
            let content = tokio::fs::read_to_string(&file)
                .await
                .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
            let response = send(
                client
                    .post(format!(
                        "{base}/v1/libraries/{}/documents",
                        encode(&library)
                    ))
                    .json(&json!({ "filename": filename, "content": content, "title": title })),
            )
            .await?;
            let value = response.json::<Value>().await?;
            println!(
                "submitted\t{}\tduplicate={}\tjob_id={}",
                value["document_path"].as_str().unwrap_or("-"),
                value["duplicate"],
                value["job_id"].as_str().unwrap_or("-"),
            );
        }
        Command::Job { library, job_id } => {
            let response = send(client.get(format!(
                "{base}/v1/libraries/{}/jobs/{}",
                encode(&library),
                encode(&job_id)
            )))
            .await?;
            let value = response.json::<Value>().await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Command::Query { library, prompt } => {
            let response = send(
                client
                    .post(format!("{base}/v1/libraries/{}/query", encode(&library)))
                    .json(&json!({ "prompt": prompt })),
            )
            .await?;
            let value = response.json::<Value>().await?;
            println!("{}", value["answer"].as_str().unwrap_or_default());
        }
    }
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

fn print_library_line(library: &Value) {
    println!(
        "{}\t{}\t{}",
        library["id"].as_str().unwrap_or("-"),
        library["name"].as_str().unwrap_or("-"),
        library["root"].as_str().unwrap_or("-"),
    );
}
