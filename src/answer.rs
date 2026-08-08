//! The query-answer contract: the JSON object the Agent emits inside its
//! `<noema-answer>` marker, the JSON Schema generated from that contract
//! (installed into each library's `AGENTS.md`, so the Agent's system prompt
//! and the server-side validator never drift), and the server-side
//! resolution that verifies declared citations against the library's files
//! and computes offsets and lines.
//!
//! The server brings no document-structure knowledge: it checks verbatim
//! strings and counts characters and newlines. Locators (`第三十三条`,
//! `5.2.1`) are labels the Agent declares and the server passes through;
//! the verified ground truth a citation stands on is always the quote plus
//! its offsets. A block that fails to parse or validate degrades to a plain
//! answer with no references — an unverified citation is worse than none
//! in a knowledge base whose value is traceability.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::{LazyLock, OnceLock},
};

use regex::Regex;

use crate::{
    error::AppError,
    models::{AgentAnswer, AgentReference, Reference},
    references::safe_knowledge_path,
};

/// The contract schema, generated once from the [`AgentAnswer`] type and
/// shared by its JSON rendering and the answer validator.
fn contract_schema() -> &'static serde_json::Value {
    static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::to_value(schemars::schema_for!(AgentAnswer)).expect("schema serializes")
    })
}

/// The contract schema as compact JSON, installed into each library's
/// generated `AGENTS.md` contract.
pub(crate) fn answer_schema_json() -> &'static str {
    static SCHEMA_JSON: OnceLock<String> = OnceLock::new();
    SCHEMA_JSON.get_or_init(|| serde_json::to_string(contract_schema()).expect("schema serializes"))
}

/// The full Noema contract, installed into each library's `AGENTS.md` and
/// thereby injected by OpenCode into the system prompt of every session in
/// that library. It defines session boundaries and the query output contract;
/// task skills contain the detailed workflows.
pub(crate) fn agents_contract() -> String {
    format!(
        r#"## Noema

### 摄入与维护

当前目录是内容库根目录。摄入或维护任务只能修改用户消息给出的 `staging/<job_id>/`；库根内容和工作区外路径不可修改。摄入任务先加载 `kb-ingest` Skill，维护任务先加载 `kb-maintain` Skill。

### 查询

查询只读取当前内容库，先加载 `kb-query` Skill。最终答案只依据库内证据；每个事实性结论以 `[n]` 标记对应的 `references` 条目。引用必须指向 `raw/` 原文，并逐字给出 `quote`。

最终答案用 `<noema-answer>` 与 `</noema-answer>` 包裹；标记之间只放一个符合以下 JSON Schema 的 JSON 对象：
{schema}
"#,
        schema = answer_schema_json()
    )
}

fn contract_validator() -> &'static jsonschema::Validator {
    static VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        jsonschema::validator_for(contract_schema()).expect("generated schema is itself valid")
    })
}

/// Parse the Agent's final answer block into the contract. Tolerates code
/// fences and stray prose: every `{` is tried as a potential object start
/// and the first span that parses as one complete value wins — prose after
/// the JSON (even prose containing braces) is ignored, and if several
/// objects parse, the first one satisfying the contract is used.
pub(crate) fn parse_agent_answer(block: &str) -> Result<AgentAnswer, AppError> {
    let mut first_violation: Option<String> = None;
    let mut cursor = 0;
    while let Some(offset) = block[cursor..].find('{') {
        let start = cursor + offset;
        cursor = start + 1;
        let Some(Ok(value)) = serde_json::Deserializer::from_str(&block[start..])
            .into_iter::<serde_json::Value>()
            .next()
        else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        if let Err(error) = contract_validator().validate(&value) {
            if first_violation.is_none() {
                first_violation = Some(error.to_string());
            }
            continue;
        }
        return Ok(serde_json::from_value(value)?);
    }
    match first_violation {
        Some(error) => Err(AppError::BadRequest(format!(
            "answer JSON violates the contract: {error}"
        ))),
        None => Err(AppError::BadRequest(
            "answer contains no JSON object".into(),
        )),
    }
}

/// Turn the Agent's raw answer block into the user-facing answer plus
/// verified references. A block that is not valid contract JSON degrades to
/// the block as-is with no references (logged): citations only ever reach
/// the client verified.
pub(crate) fn present_answer(root: &Path, block: &str) -> (String, Vec<Reference>) {
    match parse_agent_answer(block) {
        Ok(parsed) => {
            let references = resolve_references(root, &parsed.references);
            let valid = references.iter().map(|reference| reference.id).collect();
            (clean_markers(&parsed.answer, &valid), references)
        }
        Err(error) => {
            tracing::warn!(%error, "answer is not valid contract JSON; returning it without references");
            (strip_answer_markers(block), Vec::new())
        }
    }
}

/// The degradation path must not leak the marker protocol into user-facing
/// text (an agent may emit a dangling open marker and no JSON at all).
fn strip_answer_markers(block: &str) -> String {
    block
        .replace(crate::runtime::ANSWER_OPEN, "")
        .replace(crate::runtime::ANSWER_CLOSE, "")
        .trim()
        .to_string()
}

/// Resolve the Agent's declared citations against the library at `root`.
/// A citation survives only when it points at a `raw/` source document
/// (wiki nodes navigate but are never citable) and its quote is a verbatim
/// substring of that file; agent-supplied offsets are adopted only if they
/// slice out the quote exactly, and recomputed otherwise. Survivors keep
/// their 1-based array position as `id`, so dropped citations leave id gaps
/// that line up with the answer's `[n]` markers.
pub(crate) fn resolve_references(root: &Path, declared: &[AgentReference]) -> Vec<Reference> {
    let titles = document_titles(root);
    declared
        .iter()
        .enumerate()
        .filter_map(|(index, declaration)| {
            resolve_one(root, (index + 1) as u32, declaration, &titles)
        })
        .collect()
}

fn resolve_one(
    root: &Path,
    id: u32,
    declaration: &AgentReference,
    titles: &HashMap<String, String>,
) -> Option<Reference> {
    if !safe_knowledge_path(&declaration.source) || declaration.quote.is_empty() {
        tracing::warn!(source = %declaration.source, "citation rejected: not a knowledge path or empty quote");
        return None;
    }
    // Citations stand on primary evidence only: wiki nodes are compiled
    // summaries and may only navigate, never be cited. The contract tells
    // the Agent to trace a node's sources back to raw/ and cite that.
    if !declaration.source.starts_with("raw/") {
        tracing::warn!(source = %declaration.source, "citation rejected: only raw/ source documents are citable");
        return None;
    }
    // Resolve through the canonical path: the agent runs with full
    // permissions and could plant a symlink under raw/ pointing outside the
    // library; verification must never read (and "confirm") such a target.
    let Some(source_path) = crate::references::contained_file(root, &declaration.source) else {
        tracing::warn!(source = %declaration.source, "citation rejected: source escapes the library or is missing");
        return None;
    };
    let content = match fs::read_to_string(&source_path) {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!(source = %declaration.source, %error, "citation rejected: unreadable source");
            return None;
        }
    };
    let (start_byte, end_byte) = locate_quote(&content, declaration)?;
    let start = content[..start_byte].chars().count();
    let end = start + declaration.quote.chars().count();
    // The compiled wiki node matching the cited raw document, when one
    // exists: the "source → node" second hop of the traceability chain.
    let node = format!("wiki/{}.md", stem_of(&declaration.source));
    Some(Reference {
        id,
        title: titles
            .get(&declaration.source)
            .cloned()
            .unwrap_or_else(|| stem_of(&declaration.source)),
        source: declaration.source.clone(),
        locator: declaration.locator.clone(),
        quote: Some(declaration.quote.clone()),
        start: Some(start),
        end: Some(end),
        lines: Some((
            line_at(&content, start_byte),
            line_at(&content, end_byte) - usize::from(declaration.quote.ends_with('\n')),
        )),
        node: root.join(&node).is_file().then_some(node),
    })
}

/// The byte range of the declaration's quote in the source: the Agent's own
/// offsets when they slice out the quote exactly, otherwise the quote's
/// first occurrence; `None` when the quote appears nowhere, which drops the
/// citation.
fn locate_quote(content: &str, declaration: &AgentReference) -> Option<(usize, usize)> {
    if let (Some(start), Some(end)) = (declaration.start, declaration.end) {
        if let Some(byte_start) = char_to_byte(content, start)
            && content[byte_start..].starts_with(declaration.quote.as_str())
            && end == start + declaration.quote.chars().count()
        {
            return Some((byte_start, byte_start + declaration.quote.len()));
        }
        tracing::debug!(source = %declaration.source, "agent-supplied offsets do not slice out the quote; recomputing");
    }
    let mut occurrences = content.match_indices(declaration.quote.as_str());
    let (byte_start, _) = occurrences.next()?;
    if occurrences.next().is_some() {
        tracing::debug!(source = %declaration.source, "quote occurs more than once; citing the first occurrence");
    }
    Some((byte_start, byte_start + declaration.quote.len()))
}

/// Byte offset of the n-th Unicode character, or the string end when n is
/// exactly the character count.
fn char_to_byte(content: &str, characters: usize) -> Option<usize> {
    content
        .char_indices()
        .nth(characters)
        .map(|(byte, _)| byte)
        .or_else(|| (characters == content.chars().count()).then_some(content.len()))
}

/// The 1-based line number of the character at byte offset `byte`.
fn line_at(content: &str, byte: usize) -> usize {
    content[..byte].matches('\n').count() + 1
}

fn stem_of(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Display titles registered at upload time, keyed by raw path, read from
/// the library manifest. A missing or unparseable manifest yields no titles;
/// callers fall back to the filename stem.
fn document_titles(root: &Path) -> HashMap<String, String> {
    let Ok(bytes) = fs::read(root.join("manifest.json")) else {
        return HashMap::new();
    };
    let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return HashMap::new();
    };
    manifest
        .get("documents")
        .and_then(|documents| documents.as_array())
        .map(|documents| {
            documents
                .iter()
                .filter_map(|document| {
                    Some((
                        document.get("path")?.as_str()?.to_string(),
                        document.get("title")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Drop every `[n]` citation marker whose n did not survive verification;
/// surviving ids are untouched and never renumbered, so a claim whose
/// citation failed verification simply reads as uncited.
pub(crate) fn clean_markers(answer: &str, valid: &HashSet<u32>) -> String {
    // `[0-9]` rather than `\d`: ASCII digits only, matching how the Agent
    // declares reference ids (regex's `\d` is Unicode by default).
    static MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([0-9]+)\]").unwrap());
    MARKER
        .replace_all(answer, |captures: &regex::Captures<'_>| {
            let id = captures[1].parse::<u32>().unwrap_or(0);
            if valid.contains(&id) {
                captures[0].to_string()
            } else {
                String::new()
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const FIRST_ARTICLE: &str = "第一条 为保障债权的实现,制定本办法。";
    const EIGHTEENTH_ARTICLE: &str =
        "第十八条 当事人在保证合同中约定保证人与债务人对债务承担连带责任的,为连带责任保证。";
    const CONTINUATION: &str = "连带责任保证的债务人在主合同规定的债务履行期届满没有履行债务的,债权人可以要求债务人履行债务。";

    fn fixture() -> TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("raw")).unwrap();
        fs::create_dir_all(tmp.path().join("wiki")).unwrap();
        fs::write(
            tmp.path().join("raw/担保法.md"),
            format!("{FIRST_ARTICLE}\n{EIGHTEENTH_ARTICLE}\n{CONTINUATION}\n"),
        )
        .unwrap();
        fs::write(tmp.path().join("wiki/担保法.md"), "node").unwrap();
        fs::write(
            tmp.path().join("manifest.json"),
            r#"{"documents":[{"path":"raw/担保法.md","title":"中华人民共和国担保法"}]}"#,
        )
        .unwrap();
        tmp
    }

    fn declared(quote: &str) -> AgentReference {
        AgentReference {
            source: "raw/担保法.md".into(),
            quote: quote.into(),
            locator: None,
            start: None,
            end: None,
        }
    }

    fn slice_back(content: &str, start: usize, end: usize) -> String {
        content.chars().skip(start).take(end - start).collect()
    }

    #[test]
    fn parses_bare_and_fenced_contract_json() {
        let bare = r#"{"answer":"结论[1]。","references":[{"source":"raw/担保法.md","quote":"第十八条"}]}"#;
        assert!(parse_agent_answer(bare).is_ok());
        let fenced = format!("前言\n```json\n{bare}\n```\n收尾");
        let parsed = parse_agent_answer(&fenced).unwrap();
        assert_eq!(parsed.answer, "结论[1]。");
        assert_eq!(parsed.references.len(), 1);
    }

    #[test]
    fn rejects_payloads_that_violate_the_schema() {
        // Missing `answer`.
        assert!(parse_agent_answer(r#"{"references":[]}"#).is_err());
        // Missing `quote` in a reference.
        assert!(
            parse_agent_answer(r#"{"answer":"x","references":[{"source":"raw/a.md"}]}"#).is_err()
        );
        // Not JSON at all.
        assert!(parse_agent_answer("一段没有结构的散文。").is_err());
    }

    #[test]
    fn parses_contract_json_followed_by_prose_with_braces() {
        // Trailing prose with braces used to extend the naive `{...}` span
        // past the JSON and break the parse.
        let block = r#"{"answer":"结论","references":[]} 如需引用见附录{一}"#;
        assert_eq!(parse_agent_answer(block).unwrap().answer, "结论");
    }

    #[test]
    fn picks_the_first_contract_object_when_non_contract_json_precedes_it() {
        let block = r#"示例 {"foo":1} 真答案 {"answer":"正文","references":[]}"#;
        assert_eq!(parse_agent_answer(block).unwrap().answer, "正文");
    }

    #[test]
    fn resolves_quote_to_offsets_lines_title_and_node() {
        let tmp = fixture();
        let quote = "当事人在保证合同中约定保证人与债务人对债务承担连带责任的,为连带责任保证。";
        let resolved = resolve_references(tmp.path(), std::slice::from_ref(&declared(quote)));
        let reference = &resolved[0];
        let content = fs::read_to_string(tmp.path().join("raw/担保法.md")).unwrap();
        assert_eq!(reference.id, 1);
        assert_eq!(reference.title, "中华人民共和国担保法");
        assert_eq!(reference.lines, Some((2, 2)));
        assert_eq!(reference.node.as_deref(), Some("wiki/担保法.md"));
        assert_eq!(
            slice_back(&content, reference.start.unwrap(), reference.end.unwrap()),
            quote
        );
    }

    #[test]
    fn adopts_agent_offsets_that_slice_out_the_quote_and_recomputes_the_rest() {
        let tmp = fixture();
        let content = fs::read_to_string(tmp.path().join("raw/担保法.md")).unwrap();
        let quote = "为连带责任保证。";
        let real_byte = content.find(quote).unwrap();
        let real_start = content[..real_byte].chars().count();

        let mut exact = declared(quote);
        exact.start = Some(real_start);
        exact.end = Some(real_start + quote.chars().count());
        let resolved = resolve_references(tmp.path(), std::slice::from_ref(&exact));
        assert_eq!(resolved[0].start, Some(real_start));

        // Wrong offsets never reach the client: the quote is relocated.
        let mut wrong = declared(quote);
        wrong.start = Some(0);
        wrong.end = Some(3);
        let resolved = resolve_references(tmp.path(), std::slice::from_ref(&wrong));
        assert_eq!(resolved[0].start, Some(real_start));
        assert_eq!(
            slice_back(
                &content,
                resolved[0].start.unwrap(),
                resolved[0].end.unwrap()
            ),
            quote
        );
    }

    #[test]
    fn multi_line_quotes_span_the_covered_lines() {
        let tmp = fixture();
        let quote = format!("为连带责任保证。\n{CONTINUATION}");
        let resolved = resolve_references(tmp.path(), std::slice::from_ref(&declared(&quote)));
        assert_eq!(resolved[0].lines, Some((2, 3)));
    }

    #[test]
    fn drops_citations_whose_quote_is_not_verbatim_or_path_unsafe() {
        let tmp = fixture();
        let paraphrase = declared("意思相近但并非原文的改写");
        assert!(resolve_references(tmp.path(), std::slice::from_ref(&paraphrase)).is_empty());
        let unsafe_path = AgentReference {
            source: "../control.sqlite".into(),
            quote: "x".into(),
            locator: None,
            start: None,
            end: None,
        };
        assert!(resolve_references(tmp.path(), std::slice::from_ref(&unsafe_path)).is_empty());
    }

    /// Wiki nodes are compiled summaries: they may navigate the Agent to
    /// the raw/ source behind a claim, but citations stand on primary
    /// evidence only, so a wiki citation is dropped even when its quote is
    /// verbatim in the node.
    #[test]
    fn drops_citations_whose_source_is_a_wiki_node() {
        let tmp = fixture();
        fs::write(tmp.path().join("wiki/担保法.md"), "连带责任保证的定义。").unwrap();
        let declaration = AgentReference {
            source: "wiki/担保法.md".into(),
            quote: "连带责任保证的定义。".into(),
            locator: None,
            start: None,
            end: None,
        };
        assert!(resolve_references(tmp.path(), std::slice::from_ref(&declaration)).is_empty());
    }

    /// The agent runs with full permissions and could plant a link under
    /// raw/ pointing outside the library; verification must not read (and
    /// thereby "confirm") its target.
    #[test]
    #[cfg(unix)]
    fn drops_citations_whose_source_is_a_symlink_escaping_the_library() {
        let tmp = fixture();
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("raw/evil")).unwrap();
        let declaration = AgentReference {
            source: "raw/evil".into(),
            quote: "root".into(),
            locator: None,
            start: None,
            end: None,
        };
        assert!(resolve_references(tmp.path(), std::slice::from_ref(&declaration)).is_empty());
    }

    #[test]
    fn passes_locators_through_and_repeats_first_occurrence() {
        let tmp = fixture();
        let mut declaration = declared("连带责任保证");
        declaration.locator = Some("第十八条".into());
        // Occurs several times in the file: the first occurrence wins.
        let resolved = resolve_references(tmp.path(), std::slice::from_ref(&declaration));
        let reference = &resolved[0];
        assert_eq!(reference.locator.as_deref(), Some("第十八条"));
        assert_eq!(reference.lines, Some((2, 2)));
    }

    #[test]
    fn clean_markers_strips_unverified_ids_and_preserves_gaps() {
        let valid: HashSet<u32> = [2].into_iter().collect();
        assert_eq!(
            clean_markers("甲[1]。乙[2]。丙[3]。", &valid),
            "甲。乙[2]。丙。"
        );
        assert_eq!(clean_markers("无标记的答案。", &valid), "无标记的答案。");
        // An overflowing id degrades to 0 and is dropped like any
        // unverified id; adjacent markers are handled independently.
        assert_eq!(
            clean_markers("溢[99999999999999]。丁[1][2]。", &valid),
            "溢。丁[2]。"
        );
        // Non-ASCII digits are not citation markers and pass through.
        assert_eq!(clean_markers("数[١]。", &valid), "数[١]。");
    }

    #[test]
    fn the_agents_contract_embeds_the_generated_schema_and_task_skills() {
        let contract = agents_contract();
        assert!(contract.contains("AgentAnswer"), "schema embedded");
        assert!(
            contract.contains("staging/<job_id>"),
            "staging boundary embedded"
        );
        assert!(contract.contains("kb-ingest"), "ingest skill embedded");
        assert!(
            contract.contains("kb-maintain"),
            "maintenance skill embedded"
        );
        assert!(contract.contains("kb-query"), "query skill embedded");
    }

    #[test]
    fn present_answer_degrades_to_plain_text_without_references() {
        let tmp = fixture();
        let (answer, references) = present_answer(tmp.path(), "不是契约 JSON 的散文答案。");
        assert_eq!(answer, "不是契约 JSON 的散文答案。");
        assert!(references.is_empty());
    }

    #[test]
    fn degraded_answer_never_leaks_the_marker_protocol() {
        let tmp = fixture();
        let (answer, references) =
            present_answer(tmp.path(), "<noema-answer>散文答案，收尾标记丢了");
        assert_eq!(answer, "散文答案，收尾标记丢了");
        assert!(references.is_empty());
    }
}
