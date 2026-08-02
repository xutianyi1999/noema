//! noemactl: offline Noema content-library administration.
//!
//! Operates directly on the data directory (no OpenCode Server needed) and
//! supports snapshot export/import. Every import creates a brand-new, fully
//! isolated library; libraries never share files or cross-reference each
//! other.

use std::{env, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use noema::{
    snapshot::{self, ImportOptions},
    storage::Storage,
};

/// Noema 内容库管理工具：快照导入 / 导出。不同内容库完全隔离，互不共享、互不混用。
#[derive(Parser)]
#[command(name = "noemactl", version, about)]
struct Cli {
    /// 数据目录（默认取 $NOEMA_DATA_DIR，否则 ./data）
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 列出所有内容库
    List,
    /// 把一个内容库导出为快照归档（raw/wiki/reviews/graphify-out/.opencode/SQLite 的完整副本）
    Export {
        /// 内容库 id，或唯一的内容库名称
        library: String,
        /// 归档输出路径（默认 <内容库>.tar.gz）
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// 导入快照归档并始终创建一个全新的内容库（不影响任何已有内容库）
    Import {
        /// 快照归档路径（.tar.gz）
        archive: PathBuf,
        /// 新内容库名称（默认用快照记录的原名；名称只是别名，可重复）
        #[arg(long)]
        name: Option<String>,
        /// 新内容库描述
        #[arg(long)]
        description: Option<String>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("noemactl: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = cli.data_dir.unwrap_or_else(|| {
        PathBuf::from(env::var("NOEMA_DATA_DIR").unwrap_or_else(|_| "data".into()))
    });
    match cli.command {
        Command::List => {
            let storage = Storage::open(&data_dir)?;
            let libraries = storage.list_libraries()?;
            if libraries.is_empty() {
                println!("(no content libraries) data_dir={}", data_dir.display());
                return Ok(());
            }
            for library in libraries {
                println!(
                    "{}\t{}\t{}\t{}",
                    library.id,
                    library.name,
                    library.created_at.to_rfc3339(),
                    library.root
                );
            }
        }
        Command::Export { library, output } => {
            let output = output.unwrap_or_else(|| {
                PathBuf::from(format!("{}.tar.gz", library.replace(['/', '\\'], "-")))
            });
            let exported = snapshot::export_library(&data_dir, &library, &output)?;
            println!(
                "exported {}\t{} -> {}",
                exported.id,
                exported.name,
                output.display()
            );
        }
        Command::Import {
            archive,
            name,
            description,
        } => {
            let options = ImportOptions {
                data_dir,
                install_graphify: env::var("NOEMA_INSTALL_GRAPHIFY")
                    .map(|value| value != "0" && value != "false")
                    .unwrap_or(true),
                graphify_bin: env::var("GRAPHIFY_BIN").unwrap_or_else(|_| "graphify".into()),
            };
            let imported = snapshot::import_library(
                &archive,
                name.as_deref(),
                description.as_deref(),
                &options,
            )?;
            println!(
                "imported {}\t{} -> {}",
                imported.id, imported.name, imported.root
            );
        }
    }
    Ok(())
}
