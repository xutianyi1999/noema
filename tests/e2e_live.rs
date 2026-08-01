//! Live end-to-end test against a real OpenCode server, the configured test
//! model and the graphify CLI. Gated: skipped unless `NOEMA_LIVE_E2E=1`.
//!
//! ```bash
//! NOEMA_LIVE_E2E=1 OPENCODE_URL=http://127.0.0.1:4096 \
//! OPENCODE_TEST_MODEL=opencode/deepseek-v4-flash-free \
//! cargo test --test e2e_live -- --nocapture
//! ```

use std::{fs, path::PathBuf, time::Duration};

use noema::{
    AppService, Config,
    models::{CreateLibraryRequest, DocumentInput},
};
use tokio::time::sleep;

const SYNTHETIC_DOCUMENT: &str = "# 会话上下文与会话执行

## 会话上下文（Session Context）

会话上下文是一次 Agent 会话中积累的共享状态，包括系统提示、消息历史、
已授权的工具以及权限规则。它为每一步推理提供可引用的依据，并在会话结束
后被归档，而不是作为下一次会话的输入。

## 会话执行（Session Execution）

会话执行是 Agent 按照当前上下文循环调用工具、观察结果并推进任务的过程。
每一次工具调用都依赖会话上下文提供的权限和状态，同时把新的观察结果写回
上下文。

## 两者关系

会话上下文是会话执行的输入与记录载体；会话执行是会话上下文被消费和增长
的过程。二者在一次会话内互为因果，但都不会跨越会话边界复用。
";

fn live_enabled() -> bool {
    std::env::var("NOEMA_LIVE_E2E").is_ok_and(|value| value == "1")
}

/// Top-level YAML keys of the node's frontmatter block.
fn frontmatter_keys(contents: &str) -> Vec<String> {
    let Some(rest) = contents.strip_prefix("---\n") else {
        return Vec::new();
    };
    let Some(end) = rest.find("\n---\n") else {
        return Vec::new();
    };
    rest[..end]
        .lines()
        .filter(|line| {
            !line.starts_with(|character: char| character.is_whitespace())
                && line.contains(':')
                && line
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_lowercase() || character == '_')
        })
        .map(|line| line.split(':').next().unwrap_or_default().to_string())
        .collect()
}

/// Body of the node's `RAG Version` heading, whitespace-stripped, up to the
/// next Markdown heading.
fn rag_version_body(contents: &str) -> Option<String> {
    let lowered = contents.to_lowercase();
    let mut start = None;
    for needle in ["## rag version", "# rag version"] {
        if let Some(pos) = lowered.find(needle) {
            start = Some(pos + needle.len());
            break;
        }
    }
    let start = start?;
    let rest = &contents[start..];
    let end = rest.find("\n#").unwrap_or(rest.len());
    Some(
        rest[..end]
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect(),
    )
}

#[tokio::test]
async fn live_ingest_and_query_with_real_opencode() {
    if !live_enabled() {
        eprintln!("NOEMA_LIVE_E2E != 1 — skipping live end-to-end test");
        return;
    }

    // Fixed, inspectable data directory so the generated knowledge nodes and
    // graph survive the run (a TempDir would be deleted on failure).
    let data_dir =
        PathBuf::from(std::env::var("NOEMA_LIVE_DATA_DIR").unwrap_or("/tmp/noema-e2e-live".into()));
    if data_dir.exists() {
        fs::remove_dir_all(&data_dir).unwrap();
    }
    let config = Config {
        data_dir: data_dir.clone(),
        bind: "127.0.0.1:0".parse().unwrap(),
        opencode_url: std::env::var("OPENCODE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:4096".into()),
        opencode_model: std::env::var("OPENCODE_TEST_MODEL")
            .unwrap_or_else(|_| "opencode/deepseek-v4-flash-free".into()),
        opencode_timeout_secs: 1200,
        graphify_bin: std::env::var("GRAPHIFY_BIN").unwrap_or_else(|_| "graphify".into()),
        install_graphify: true,
    };
    println!("model under test: {}", config.opencode_model);
    let service = AppService::new(config).unwrap();

    // Library creation runs the upstream graphify installer.
    let library = service
        .create_library(CreateLibraryRequest {
            name: "Live 端到端".into(),
            description: Some("真实 OpenCode 驱动的端到端验证".into()),
        })
        .await
        .expect("library creation (including graphify install) must succeed");
    let root = PathBuf::from(&library.root);
    for artifact in [
        ".opencode/skills/graphify/SKILL.md",
        ".opencode/plugins/graphify.js",
        ".opencode/opencode.json",
        ".opencode/skills/kb-ingest/SKILL.md",
        ".opencode/skills/kb-query/SKILL.md",
        ".opencode/skills/knowledge-compiler/SKILL.md",
        "purpose.md",
        "schema.md",
        ".graphifyignore",
    ] {
        assert!(
            root.join(artifact).is_file(),
            "missing {artifact} after library creation"
        );
    }
    println!("library {} created at {}", library.id, root.display());

    // Ingest a synthetic two-concept document through the real agent.
    let submitted = service
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "session-design.md".into(),
                content: SYNTHETIC_DOCUMENT.into(),
                title: Some("会话设计".into()),
            },
        )
        .await
        .unwrap();
    assert!(!submitted.duplicate);

    let deadline = std::time::Instant::now() + Duration::from_secs(20 * 60);
    let job = loop {
        let status = service.job_status(&library.id, &submitted.job_id).unwrap();
        if matches!(status.status.as_str(), "completed" | "failed") {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ingestion did not finish within 20 minutes"
        );
        sleep(Duration::from_secs(3)).await;
    };
    assert_eq!(
        job.status,
        "completed",
        "live ingestion failed: {job:?}\nstaging left for inspection under {}",
        root.join("staging").display()
    );
    println!("ingestion completed in session {:?}", job.session_id);

    // The agent must have produced knowledge nodes honoring the full
    // knowledge-compiler contract: the 9 frontmatter keys from PLAN §4 and
    // the body sections (definition, evidence, example, limits, RAG Version,
    // references).
    let wiki_files: Vec<PathBuf> = fs::read_dir(root.join("wiki"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    assert!(
        !wiki_files.is_empty(),
        "agent produced no wiki nodes; staging: {}",
        root.join("staging").display()
    );
    for node in &wiki_files {
        let contents = fs::read_to_string(node).unwrap();
        for key in [
            "node_id:",
            "canonical_name:",
            "kind:",
            "sources:",
            "relations:",
            "claim_type:",
            "confidence:",
            "created_at:",
            "updated_at:",
        ] {
            assert!(
                contents.contains(key),
                "{} misses frontmatter key {key}",
                node.display()
            );
        }
        let lowered = contents.to_lowercase();
        assert!(
            contents.starts_with("---\n"),
            "{} must open with YAML frontmatter",
            node.display()
        );
        assert!(
            lowered.contains("rag version"),
            "{} misses RAG Version section:\n{contents}",
            node.display()
        );
        // The contract is exactly the 9 frontmatter keys: agents must not
        // invent extras such as `rag_version:` or `aliases:`.
        for key in frontmatter_keys(&contents) {
            assert!(
                [
                    "node_id",
                    "canonical_name",
                    "kind",
                    "sources",
                    "relations",
                    "claim_type",
                    "confidence",
                    "created_at",
                    "updated_at",
                ]
                .contains(&key.as_str()),
                "{} has frontmatter key `{key}` outside the 9-key contract:\n{contents}",
                node.display()
            );
        }
        // RAG Version must be a substantive compressed summary (PLAN §4:
        // 100–300 chars), not a one-line version changelog entry.
        let rag = rag_version_body(&contents).unwrap_or_default();
        assert!(
            rag.chars().count() >= 60,
            "{} RAG Version must be a compressed summary, got {rag:?}:\n{contents}",
            node.display()
        );
        for (section, needles) in [
            ("definition", &["定义", "definition"][..]),
            ("evidence", &["证据", "推理", "evidence", "reasoning"][..]),
            ("example", &["示例", "反例", "example"][..]),
            ("limits", &["局限", "limitation"][..]),
            ("references", &["引用", "参考", "reference"][..]),
        ] {
            assert!(
                needles
                    .iter()
                    .any(|needle| lowered.contains(&needle.to_lowercase())),
                "{} misses {section} section:\n{contents}",
                node.display()
            );
        }
        // Nodes must back-link to an existing raw source.
        assert!(
            contents.contains("raw/") && lowered.contains("session-design"),
            "{} misses source back-link to the ingested document:\n{contents}",
            node.display()
        );
        println!("wiki node OK: {}", node.display());
    }

    // graphify must have built its graph inside the library boundary.
    let graph_path = root.join("graphify-out/graph.json");
    assert!(
        graph_path.is_file(),
        "graphify-out/graph.json was not built"
    );
    let graph: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&graph_path).unwrap()).unwrap();
    assert!(
        graph.get("nodes").is_some() && graph.get("links").is_some(),
        "unexpected graphify schema: {}",
        &graph.to_string()[..graph.to_string().len().min(200)]
    );
    println!(
        "graphify graph: {} nodes / {} links",
        graph["nodes"].as_array().map(Vec::len).unwrap_or(0),
        graph["links"].as_array().map(Vec::len).unwrap_or(0)
    );

    // Two real queries: fresh sessions, grounded answers.
    let first = service
        .query(&library.id, "会话上下文和会话执行是什么关系？请引用来源。")
        .await
        .expect("first live query must succeed");
    assert!(!first.answer.trim().is_empty(), "empty answer");
    println!("--- first answer ---\n{}", first.answer);
    println!("references: {:?}", first.references);
    assert!(
        !first.references.is_empty(),
        "answer without grounded references: {}",
        first.answer
    );

    let second = service
        .query(&library.id, "会话结束后上下文还能被下一次会话看到吗？")
        .await
        .expect("second live query must succeed");
    println!("--- second answer ---\n{}", second.answer);
    assert_ne!(
        first.session_id, second.session_id,
        "queries must not share an OpenCode session"
    );
    assert!(
        !second.answer.contains(&first.answer),
        "second session must not reuse first session output"
    );

    println!(
        "live e2e OK: sessions {} and {}",
        first.session_id, second.session_id
    );
}

#[cfg(test)]
mod contract_helpers {
    use super::*;

    const NODE: &str = "---\nnode_id: a\ncanonical_name: A\nsources:\n  - path: raw/x.md\n    locator: \"h\"\n---\n\n## 定义\n\n…\n\n## RAG Version\n\n会话上下文是一次会话中积累的共享状态，会话执行按其循环调用工具并写回观察，二者在一次会话内互为因果，但不跨越会话边界复用，来源见 raw/x.md 与相关 wiki 节点。\n\n## 引用\n\nraw/x.md\n";

    #[test]
    fn frontmatter_keys_lists_only_top_level_keys() {
        assert_eq!(
            frontmatter_keys(NODE),
            ["node_id", "canonical_name", "sources"]
        );
    }

    #[test]
    fn rag_version_body_extracts_only_that_section() {
        let body = rag_version_body(NODE).unwrap();
        assert!(body.contains("不跨越会话边界复用"));
        assert!(!body.contains("引用"));
        assert!(body.chars().count() >= 60);
    }
}
