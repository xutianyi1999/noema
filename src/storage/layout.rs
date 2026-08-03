//! The on-disk shape of one content library: the directory/file layout
//! shared by creation, staging and promotion, and the seed files written
//! at creation.

use std::{fs, path::Path};

use super::{documents, fsutil::write_atomic};
use crate::error::AppError;

/// The on-disk layout of one content library — the single source of truth
/// for creation, staging preparation, promotion and staging validation.
pub(super) struct LibraryLayout;

impl LibraryLayout {
    /// Directories created for a brand-new library.
    pub(super) const SCAFFOLD_DIRS: [&'static str; 6] = [
        ".opencode",
        "raw",
        "wiki",
        "reviews",
        "staging",
        "graphify-out",
    ];

    /// Top-level entries copied into a staging workspace. `AGENTS.md`
    /// appears here but not in `SCAFFOLD_DIRS`: the graphify installer
    /// creates it at bootstrap, after the scaffold.
    pub(super) const STAGING_INPUTS: [&'static str; 11] = [
        ".opencode",
        ".graphifyignore",
        "AGENTS.md",
        "purpose.md",
        "schema.md",
        "index.md",
        "raw",
        "wiki",
        "reviews",
        "manifest.json",
        "graphify-out",
    ];

    /// Staging directories promoted back over the library root, replacing
    /// whatever is there wholesale.
    pub(super) const PROMOTED_DIRS: [&'static str; 3] = ["wiki", "reviews", "graphify-out"];

    /// Staging files promoted back over the library root, copied
    /// individually.
    pub(super) const PROMOTED_FILES: [&'static str; 2] = ["index.md", "manifest.json"];

    /// Paths the ingestion agent must not modify; staging is compared
    /// against the library baseline for each of them.
    pub(super) const PROTECTED_PATHS: [&'static str; 4] =
        [".graphifyignore", "raw", "purpose.md", "schema.md"];
}

/// Create the directory scaffold and seed files for a brand-new library.
pub(super) fn scaffold_library(root: &Path, name: &str) -> Result<(), AppError> {
    for directory in LibraryLayout::SCAFFOLD_DIRS {
        fs::create_dir_all(root.join(directory))?;
    }
    write_atomic(&root.join("purpose.md"), default_purpose(name).as_bytes())?;
    write_atomic(&root.join("schema.md"), default_schema().as_bytes())?;
    write_atomic(&root.join("index.md"), default_index(name).as_bytes())?;
    write_atomic(
        &root.join(".graphifyignore"),
        graphify_scope_ignore().as_bytes(),
    )?;
    write_atomic(&root.join("manifest.json"), b"{\n  \"documents\": []\n}\n")?;
    documents::init_library_db(&root.join("library.sqlite"))
}

fn default_purpose(name: &str) -> String {
    format!(
        "# {name}\n\n在此定义内容库的目的、范围、关键问题与术语。说明来源的权威层级（如：法律法规 > 司法解释 > 标准指南 > 其他文档）与更新策略，包括被废止或修订的文本如何标注，以区别于现行有效的文本。\n"
    )
}

fn graphify_scope_ignore() -> &'static str {
    "# Noema graphify input boundary: only text sources and compiled nodes.\n*\n!raw/\n!raw/**\n!wiki/\n!wiki/**\n"
}

fn default_schema() -> String {
    "# Knowledge Schema\n\n\
     知识节点契约（详见 knowledge-compiler Skill）：frontmatter 恰好包含 9 个键 —— \
     node_id（库内稳定标识）、canonical_name、kind（concept|entity|process|decision|issue）、\
     sources（raw/ 下的 path + locator，locator 按来源自身编号标注，如 第三十三条第二款 或 5.2.1）、\
     relations（depends_on / related_to / opposite_to）、\
     claim_type（observed|summarized|inferred|unresolved）、confidence（0.0–1.0）、\
     created_at、updated_at（RFC-3339）；不要添加额外键。\n\n\
     正文包含 6 个小节：定义、证据/推理、示例或反例、局限性、\
     RAG Version（100–300 字的高密度压缩摘要，不是版本变更记录）、\
     引用（相对路径 + locator，如 raw/law.md 第三十三条 或 wiki/concept.md）。\
     法条、合同条款、公文与标准原文等规范文本必须从 raw/ 原文逐字引用，不得改写；\
     RAG Version 只压缩节点的评述与关系，不改写规范文本。\
     未解决的声明、新旧文本矛盾放入 reviews/，并用 opposite_to 表达。\n"
        .into()
}

fn default_index(name: &str) -> String {
    format!("# {name}\n\n尚未摄入任何文档。\n")
}
