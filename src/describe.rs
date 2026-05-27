//! Text collection and LLM description generation for RPM spec `%description`.
//!
//! `collect_description_input` walks a directory tree and extracts structured
//! metadata from PKG-INFO / METADATA / pyproject.toml / setup.cfg / setup.py,
//! a README excerpt (badge lines skipped, max 2500 chars), and a list of
//! Python modules inferred from the source layout.
//!
//! `generate_description_silent` sends a system + user prompt to Ollama's
//! `/api/chat` endpoint via `ollama_rs` and returns the cleaned result.

use std::{collections::BTreeSet, fs, path::Path};

use anyhow::Result;
use serde::Serialize;
use tracing::{debug, warn};

// ── structured input ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct DescriptionInput {
    pub metadata: PackageMetadata,
    pub core_modules: Vec<String>,
    pub sources: SourceFiles,
    pub readme_excerpt: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PackageMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requires_dist: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SourceFiles {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pyproject_toml: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_cfg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_py: Option<String>,
}

// ── directory walking ─────────────────────────────────────────────────

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "build",
    "dist",
    ".tox",
    ".venv",
    "__pycache__",
    "logs",
    ".packfix",
];

const README_EXCERPT_MAX: usize = 2500;

/// Walk `root_dir`, parse metadata and README, build a structured input.
pub fn collect_description_input(root_dir: &Path) -> Result<DescriptionInput> {
    // Phase 1: read PKG-INFO / METADATA
    let mut metadata = PackageMetadata::default();
    if let Some(text) = read_file(root_dir, "PKG-INFO").or_else(|| read_file(root_dir, "METADATA"))
    {
        parse_email_metadata(&text, &mut metadata);
    }

    // Phase 2: read pyproject.toml
    let sources_pyproject = read_file(root_dir, "pyproject.toml");
    if let Some(ref text) = sources_pyproject {
        parse_pyproject_toml(text, &mut metadata);
    }

    // Phase 3: read setup.cfg
    let sources_setup_cfg = read_file(root_dir, "setup.cfg");
    if let Some(ref text) = sources_setup_cfg {
        parse_setup_cfg(text, &mut metadata);
    }

    // Phase 4: read setup.py
    let sources_setup_py = read_file(root_dir, "setup.py");

    // Phase 5: README excerpt (max 2500 chars, skip badge lines)
    let readme_excerpt = read_file(root_dir, "README.md")
        .or_else(|| read_file(root_dir, "README.rst"))
        .or_else(|| read_file(root_dir, "README.txt"))
        .or_else(|| read_file(root_dir, "README"))
        .map(|text| excerpt_readme(&text))
        .unwrap_or_default();

    // Phase 6: collect module names from __init__.py files (depth 2)
    let core_modules = collect_core_modules(root_dir);

    let sources = SourceFiles {
        pyproject_toml: sources_pyproject.map(|s| cap_chars(&s, 3000)),
        setup_cfg: sources_setup_cfg.map(|s| cap_chars(&s, 3000)),
        setup_py: sources_setup_py.map(|s| cap_chars(&s, 3000)),
    };

    Ok(DescriptionInput {
        metadata,
        core_modules,
        sources,
        readme_excerpt,
    })
}

// ── JSON context ──────────────────────────────────────────────────────

/// Serialize `input` as compact JSON, capped at `max_chars`.
pub fn build_json_context(input: &DescriptionInput, max_chars: usize) -> String {
    let json = serde_json::to_string(input).unwrap_or_default();
    cap_chars(&json, max_chars)
}

/// Build the full user-prompt by substituting `{context}` with the JSON.
pub fn build_user_prompt(template: &str, context_json: &str) -> String {
    template.replace("{context}", context_json)
}

// ── Ollama host normalization ──────────────────────────────────────────

/// Normalize an Ollama host URL for use with `ollama_rs::Ollama::new`.
///
/// If the host ends with `/api/chat` or `/api/generate`, those suffixes are
/// stripped and a warning is logged.  The `ollama_rs` crate expects a base
/// URL such as `http://100.65.29.50`.
pub fn normalize_ollama_host(host: &str) -> String {
    let trimmed = host.trim_end_matches('/');
    if let Some(base) = trimmed
        .strip_suffix("/api/chat")
        .or_else(|| trimmed.strip_suffix("/api/generate"))
    {
        warn!(
            original = %host,
            normalized = %base,
            "ollama host contained /api/chat or /api/generate; using base URL"
        );
        base.to_string()
    } else {
        host.to_string()
    }
}

// ── output formatting ─────────────────────────────────────────────────

/// Clean LLM output for `%description`: 1–5 lines, max 100 chars per line,
/// no markdown, no leading/trailing whitespace.
pub fn format_spec_description(raw: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip lines that look like markdown headings, code fences, quotes
        if trimmed.starts_with('#') || trimmed.starts_with('>') || trimmed.starts_with("```") {
            continue;
        }
        // Skip lines that contain the %description tag
        if trimmed.contains("%description") {
            continue;
        }
        // Hard-wrap at 100 chars
        let mut remaining = trimmed;
        while !remaining.is_empty() {
            if remaining.len() <= 100 {
                lines.push(remaining.to_string());
                break;
            }
            // Find last space before column 100
            let split_at = match remaining[..100].rfind(' ') {
                Some(pos) if pos > 40 => pos, // only break on spaces, not mid-word unless forced
                _ => 100,
            };
            lines.push(remaining[..split_at].trim_end().to_string());
            remaining = remaining[split_at..].trim_start();
        }
    }

    // Keep at most 5 lines
    lines.truncate(5);
    lines.join("\n")
}

// ── Ollama chat call ───────────────────────────────────────────────────

/// Send a system + user prompt to Ollama chat and return the response text.
/// Returns empty string on failure (logs the real error).
pub async fn generate_description_silent(
    host: String,
    port: u16,
    model: String,
    system_prompt: String,
    user_prompt: String,
    num_predict: i32,
    temperature: f32,
) -> String {
    match generate_description_chat(
        host,
        port,
        model,
        system_prompt,
        user_prompt,
        num_predict,
        temperature,
    )
    .await
    {
        Ok(text) => text,
        Err(e) => {
            warn!(error = %e, "LLM description generation failed");
            String::new()
        }
    }
}

async fn generate_description_chat(
    host: String,
    port: u16,
    model: String,
    system_prompt: String,
    user_prompt: String,
    num_predict: i32,
    temperature: f32,
) -> Result<String> {
    use ollama_rs::Ollama;
    use ollama_rs::generation::{
        chat::{ChatMessage, request::ChatMessageRequest},
        parameters::{KeepAlive, ThinkType, TimeUnit},
    };
    use ollama_rs::models::ModelOptions;

    let ollama = Ollama::new(&host, port);

    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::user(user_prompt),
    ];

    let request = ChatMessageRequest::new(model, messages)
        .options(
            ModelOptions::default()
                .temperature(temperature)
                .num_predict(num_predict),
        )
        .keep_alive(KeepAlive::Until {
            time: 30,
            unit: TimeUnit::Minutes,
        })
        .think(ThinkType::False);

    let response = ollama.send_chat_messages(request).await?;
    let text = response.message.content.trim().to_string();

    if text.is_empty() {
        debug!("LLM returned empty description");
    } else {
        debug!(description_len = text.len(), "LLM description generated");
    }

    Ok(text)
}

// ── core generation (no networking wrappers) ──────────────────────────

/// Core description generation: collect text from `dir`, build prompts,
/// call Ollama chat, and format the result.
///
/// Does NOT include DNS pre-check or timeout wrapping — callers are
/// responsible for those (if desired).
#[allow(clippy::too_many_arguments)]
pub async fn generate_description_core(
    dir: &Path,
    host: &str,
    port: u16,
    model: &str,
    system_prompt: &str,
    user_prompt: &str,
    max_context_chars: usize,
    num_predict: i32,
    temperature: f32,
) -> String {
    let input = match collect_description_input(dir) {
        Ok(input) => {
            debug!(
                root = %dir.display(),
                name = ?input.metadata.name,
                modules = input.core_modules.len(),
                readme_chars = input.readme_excerpt.chars().count(),
                "collected description input"
            );
            input
        }
        Err(e) => {
            warn!(error = %e, dir = %dir.display(), "failed to collect description input");
            return String::new();
        }
    };

    let ctx = build_json_context(&input, max_context_chars);
    if ctx.trim().is_empty() || ctx == "{}" {
        warn!(dir = %dir.display(), "no text collected for description; skipping LLM call");
        return String::new();
    }

    let user_prompt_filled = build_user_prompt(user_prompt, &ctx);

    let raw = generate_description_silent(
        host.to_string(),
        port,
        model.to_string(),
        system_prompt.to_string(),
        user_prompt_filled,
        num_predict,
        temperature,
    )
    .await;

    let formatted = format_spec_description(&raw);
    if formatted.is_empty() && !raw.is_empty() {
        warn!(dir = %dir.display(), "description formatting produced empty result (raw {} chars)", raw.len());
    }
    formatted
}

// ── public entry-point ────────────────────────────────────────────────

/// Collect text from `dir`, build system + user prompts, call Ollama chat,
/// format the result, and return the description string.
#[allow(clippy::too_many_arguments)]
pub async fn run_describe(
    dir: &Path,
    host: &str,
    port: u16,
    model: &str,
    system_prompt: &str,
    user_prompt: &str,
    timeout_secs: u64,
    max_context_chars: usize,
    num_predict: i32,
    temperature: f32,
) -> String {
    let normalized_host = normalize_ollama_host(host);

    // Strip http:// prefix for DNS pre-check (same pattern as workflow.rs)
    let host_clean = normalized_host
        .strip_prefix("http://")
        .or_else(|| normalized_host.strip_prefix("https://"))
        .unwrap_or(&normalized_host);

    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let tcp_timeout = Duration::from_millis(300);
    if let Ok(mut addrs_iter) = (host_clean, port).to_socket_addrs()
        && let Some(sock) = addrs_iter.next()
        && TcpStream::connect_timeout(&sock, tcp_timeout).is_err()
    {
        warn!("Ollama not reachable at {host_clean}:{port}; skipping description generation");
        return String::new();
    }

    let llm_timeout = Duration::from_secs(timeout_secs);
    match tokio::time::timeout(
        llm_timeout,
        generate_description_core(
            dir,
            &normalized_host,
            port,
            model,
            system_prompt,
            user_prompt,
            max_context_chars,
            num_predict,
            temperature,
        ),
    )
    .await
    {
        Ok(description) => description,
        Err(_elapsed) => {
            warn!(
                "LLM description generation timed out after {:?}",
                llm_timeout
            );
            String::new()
        }
    }
}

// ── metadata parsing ──────────────────────────────────────────────────

fn read_file(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join(name);
    if !path.is_file() {
        return None;
    }
    let content = fs::read(&path).ok()?;
    String::from_utf8(content).ok()
}

fn parse_email_metadata(text: &str, m: &mut PackageMetadata) {
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match key.trim().to_lowercase().as_str() {
                "name" => m.name = Some(value.to_string()),
                "version" => m.version = Some(value.to_string()),
                "summary" => m.summary = Some(value.to_string()),
                "description" => m.description = Some(value.to_string()),
                "home-page" | "url" => m.home_page = Some(value.to_string()),
                "author" | "author-email" => {
                    if m.author.is_none() {
                        m.author = Some(value.to_string());
                    }
                }
                "requires-dist" => m.requires_dist.push(value.to_string()),
                _ => {}
            }
        }
    }
}

fn parse_pyproject_toml(text: &str, m: &mut PackageMetadata) {
    // Simple extraction: look for [project] table entries
    let mut in_project = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[project]" {
            in_project = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_project = false;
            continue;
        }
        if !in_project {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = clean_toml_value(value.trim());
            if value.is_empty() {
                continue;
            }
            match key {
                "name" => {
                    if m.name.is_none() {
                        m.name = Some(value);
                    }
                }
                "version" => {
                    if m.version.is_none() {
                        m.version = Some(value);
                    }
                }
                "description" => {
                    if m.summary.is_none() {
                        m.summary = Some(value);
                    }
                }
                _ => {}
            }
        }
    }
}

fn parse_setup_cfg(text: &str, m: &mut PackageMetadata) {
    let mut in_metadata = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[metadata]" {
            in_metadata = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_metadata = false;
            continue;
        }
        if !in_metadata {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if value.is_empty() {
                continue;
            }
            match key {
                "name" => {
                    if m.name.is_none() {
                        m.name = Some(value.to_string());
                    }
                }
                "version" => {
                    if m.version.is_none() {
                        m.version = Some(value.to_string());
                    }
                }
                "description" => {
                    if m.summary.is_none() {
                        m.summary = Some(value.to_string());
                    }
                }
                "long_description" => {
                    if m.description.is_none() {
                        m.description = Some(value.to_string());
                    }
                }
                _ => {}
            }
        }
    }
}

fn clean_toml_value(raw: &str) -> String {
    raw.trim_matches('"').trim_matches('\'').trim().to_string()
}

fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

// ── README excerpt ────────────────────────────────────────────────────

fn excerpt_readme(text: &str) -> String {
    let mut result = String::with_capacity(README_EXCERPT_MAX);
    for line in text.lines() {
        if result.chars().count() >= README_EXCERPT_MAX {
            break;
        }
        let trimmed = line.trim();
        // Skip badge lines (markdown image links at the top of README)
        if is_badge_line(trimmed) {
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }
    cap_chars(&result, README_EXCERPT_MAX)
}

fn is_badge_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    let s = line.trim();
    // Lines that are only markdown images: ![badge](url) or [![badge](url)](url)
    if s.starts_with("[![") || (s.starts_with("![") && s.ends_with(')')) {
        return true;
    }
    // GitHub actions badge URLs
    if s.contains("github.com/") && s.contains("/actions/workflows/") && s.contains("/badge.") {
        return true;
    }
    // Common CI badge hosts
    if (s.contains("travis-ci.")
        || s.contains("circleci.")
        || s.contains("codecov.")
        || s.contains("coveralls."))
        && s.contains(".svg")
    {
        return true;
    }
    false
}

// ── module collection ─────────────────────────────────────────────────

fn collect_core_modules(root_dir: &Path) -> Vec<String> {
    let mut modules = BTreeSet::new();
    collect_modules_recursive(root_dir, root_dir, 0, &mut modules);
    let mut out: Vec<String> = modules.into_iter().collect();
    out.sort();
    out
}

fn collect_modules_recursive(
    root_dir: &Path,
    current: &Path,
    depth: usize,
    modules: &mut BTreeSet<String>,
) {
    if depth > 2 {
        return;
    }

    // Skip known non-module directories
    let dir_name = current.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if depth > 0 && SKIP_DIRS.contains(&dir_name) {
        return;
    }
    if dir_name.starts_with('.') && depth > 0 {
        return;
    }

    let entries = match fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if name.starts_with('.') || name.starts_with('_') {
            continue;
        }

        if path.is_dir() {
            // Check for __init__.py — this is a Python package
            if path.join("__init__.py").is_file() {
                // Compute relative path from root for module name
                if let Ok(rel) = path.strip_prefix(root_dir) {
                    let mod_name = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(".");
                    if !mod_name.is_empty()
                        && !mod_name.starts_with("test")
                        && !mod_name.contains(".test")
                        && !mod_name.contains(".tests")
                        && mod_name != "docs"
                        && mod_name != "examples"
                    {
                        modules.insert(mod_name);
                    }
                }
                // Recurse into package dirs too — sub-packages exist.
            }
            collect_modules_recursive(root_dir, &path, depth + 1, modules);
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn parses_pkg_info_metadata() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "PKG-INFO",
            "Name: fonttools\nVersion: 4.62.1\nSummary: Tools for fonts\nHome-page: https://github.com/fonttools\nAuthor: Behdad Esfahbod\nRequires-Dist: lxml\n",
        );

        let input = collect_description_input(dir.path()).unwrap();
        let m = &input.metadata;
        assert_eq!(m.name.as_deref(), Some("fonttools"));
        assert_eq!(m.version.as_deref(), Some("4.62.1"));
        assert_eq!(m.summary.as_deref(), Some("Tools for fonts"));
        assert_eq!(m.home_page.as_deref(), Some("https://github.com/fonttools"));
        assert_eq!(m.author.as_deref(), Some("Behdad Esfahbod"));
        assert!(m.requires_dist.contains(&"lxml".to_string()));
    }

    #[test]
    fn parses_pyproject_toml() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            "[project]\nname = \"mypy\"\ndescription = \"A type checker\"\n",
        );

        let input = collect_description_input(dir.path()).unwrap();
        assert_eq!(input.metadata.name.as_deref(), Some("mypy"));
        assert_eq!(input.metadata.summary.as_deref(), Some("A type checker"));
        assert!(input.sources.pyproject_toml.is_some());
    }

    #[test]
    fn parses_setup_cfg() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "setup.cfg",
            "[metadata]\nname = numpy\ndescription = Array processing\n",
        );

        let input = collect_description_input(dir.path()).unwrap();
        assert_eq!(input.metadata.name.as_deref(), Some("numpy"));
        assert_eq!(input.metadata.summary.as_deref(), Some("Array processing"));
        assert!(input.sources.setup_cfg.is_some());
    }

    #[test]
    fn readme_excerpt_skips_badges() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "README.md",
            "![build](https://github.com/fonttools/actions/workflows/ci.yml/badge.svg)\n[![Coverage](https://codecov.io/gh/fonttools/fonttools/branch/main/graph/badge.svg)](https://codecov.io)\n\n# FontTools\n\nFontTools is a library for manipulating fonts.\n",
        );

        let input = collect_description_input(dir.path()).unwrap();
        assert!(!input.readme_excerpt.contains("badge.svg"));
        assert!(!input.readme_excerpt.contains("codecov.io"));
        assert!(input.readme_excerpt.contains("FontTools"));
    }

    #[test]
    fn readme_excerpt_capped_at_2500() {
        let dir = tempdir().unwrap();
        let big = "x".repeat(3000);
        write(dir.path(), "README.md", &big);

        let input = collect_description_input(dir.path()).unwrap();
        assert!(input.readme_excerpt.chars().count() <= README_EXCERPT_MAX);
    }

    #[test]
    fn collects_core_modules() {
        let dir = tempdir().unwrap();
        write(dir.path(), "fonttools/__init__.py", "");
        write(dir.path(), "fonttools/ttLib/__init__.py", "");
        write(dir.path(), "fonttools/merge.py", ""); // not a package, ignored

        let modules = collect_core_modules(dir.path());
        assert!(modules.contains(&"fonttools".to_string()));
        assert!(modules.contains(&"fonttools.ttLib".to_string()));
        assert!(!modules.contains(&"fonttools.merge".to_string()));
    }

    #[test]
    fn skips_test_modules() {
        let dir = tempdir().unwrap();
        write(dir.path(), "mypackage/__init__.py", "");
        write(dir.path(), "tests/__init__.py", "");
        write(dir.path(), "mypackage/tests/__init__.py", "");

        let modules = collect_core_modules(dir.path());
        assert!(modules.contains(&"mypackage".to_string()));
        assert!(!modules.iter().any(|m| m.contains("test")));
    }

    #[test]
    fn format_spec_description_wraps_long_lines() {
        let raw = "FontTools is a comprehensive library for manipulating fonts, providing both high-level and low-level APIs for various font formats and operations.\n";
        let formatted = format_spec_description(raw);
        for line in formatted.lines() {
            assert!(line.len() <= 100, "line too long: {line}");
        }
    }

    #[test]
    fn format_spec_description_truncates_to_5_lines() {
        let raw = "Line 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\n";
        let formatted = format_spec_description(raw);
        assert_eq!(formatted.lines().count(), 5);
    }

    #[test]
    fn format_spec_description_strips_markdown() {
        let raw = "# Heading\n> quote\n```\ncode\n```\nActual description here.\n";
        let formatted = format_spec_description(raw);
        assert!(!formatted.contains('#'));
        assert!(!formatted.contains('>'));
        assert!(!formatted.contains("```"));
        assert!(formatted.contains("Actual description here"));
    }

    #[test]
    fn build_json_context_caps_at_max() {
        let input = DescriptionInput {
            metadata: PackageMetadata {
                name: Some("test".into()),
                summary: Some("x".repeat(5000)),
                ..Default::default()
            },
            core_modules: vec![],
            sources: SourceFiles::default(),
            readme_excerpt: String::new(),
        };
        let ctx = build_json_context(&input, 200);
        assert!(ctx.chars().count() <= 200);
    }

    #[test]
    fn normalize_ollama_host_strips_api_paths() {
        assert_eq!(
            normalize_ollama_host("http://100.65.29.50/api/chat"),
            "http://100.65.29.50"
        );
        assert_eq!(
            normalize_ollama_host("http://100.65.29.50/api/generate"),
            "http://100.65.29.50"
        );
    }

    #[test]
    fn normalize_ollama_host_preserves_base_url() {
        assert_eq!(
            normalize_ollama_host("http://100.65.29.50"),
            "http://100.65.29.50"
        );
        assert_eq!(normalize_ollama_host("100.65.29.50"), "100.65.29.50");
    }

    #[test]
    fn normalize_ollama_host_strips_trailing_slash() {
        assert_eq!(
            normalize_ollama_host("http://100.65.29.50/api/chat/"),
            "http://100.65.29.50"
        );
    }

    #[test]
    fn build_user_prompt_substitutes_context() {
        let template = "Task: {context}\n";
        let filled = build_user_prompt(template, r#"{"name":"test"}"#);
        assert!(filled.contains(r#"{"name":"test"}"#));
        assert!(!filled.contains("{context}"));
    }
}
