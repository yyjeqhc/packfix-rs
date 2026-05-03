use anyhow::{Result, bail};
use ollama_rs::{Ollama, generation::completion::request::GenerationRequest};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextSuggestion {
    pub summary: Option<String>,
    pub description: Option<String>,
    pub license_suggestion: Option<String>,
    pub confidence: f32,
    pub notes: Vec<String>,
}

impl Default for TextSuggestion {
    fn default() -> Self {
        Self {
            summary: None,
            description: None,
            license_suggestion: None,
            confidence: 0.0,
            notes: vec!["llm unavailable — skipped".into()],
        }
    }
}

pub async fn summarize_spec_text_silent(
    host: String,
    port: u16,
    model: String,
    metadata_text: String,
    readme_text: String,
) -> TextSuggestion {
    summarize_spec_text(host, port, model, metadata_text, readme_text)
        .await
        .unwrap_or_default()
}

pub async fn summarize_spec_text(
    host: String,
    port: u16,
    model: String,
    metadata_text: String,
    readme_text: String,
) -> Result<TextSuggestion> {
    let ollama = Ollama::new(host, port);
    let prompt = format!(
        r#"You are helping clean Python RPM package metadata.
Return JSON only, with keys:
summary: string|null
description: string|null
license_suggestion: string|null
confidence: number
notes: string[]

Do not propose build fixes. Do not mention BuildRequires.
License is only a suggestion and will not be auto-applied.

METADATA:
{metadata_text}

README:
{readme_text}
"#
    );
    let request = GenerationRequest::new(model, prompt);
    let response = ollama.generate(request).await?;
    let cleaned = clean_llm_json(&response.response)?;
    serde_json::from_str::<TextSuggestion>(&cleaned)
        .map_err(|e| anyhow::anyhow!("failed to parse TextSuggestion from JSON: {e}"))
}

fn clean_llm_json(s: &str) -> Result<String> {
    let s = s.trim();

    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .map(|rest| rest.strip_suffix("```").unwrap_or(rest))
        .unwrap_or(s)
        .trim()
        .to_string();

    if let Some(extracted) = extract_first_json_object(&s) {
        return Ok(extracted);
    }

    bail!("no JSON object found in LLM response")
}

fn extract_first_json_object(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let mut depth: i32 = 0;
    for (i, ch) in s[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..start + i + 1].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_json_extracts_first_valid_json() {
        let s = r#"Here is the suggestion:
{"summary": "A test", "description": null, "license_suggestion": null, "confidence": 0.9, "notes": []}
Some extra text."#;
        let cleaned = clean_llm_json(s).expect("should extract JSON");
        let parsed: serde_json::Value =
            serde_json::from_str(&cleaned).expect("should be valid JSON");
        assert_eq!(parsed["summary"], "A test");
        assert_eq!(parsed["confidence"], 0.9);
    }

    #[test]
    fn clean_json_strips_markdown_fences() {
        let s = r#"```json
{"summary": "A test", "description": null, "license_suggestion": null, "confidence": 0.9, "notes": []}
```"#;
        let cleaned = clean_llm_json(s).expect("should extract JSON");
        let parsed: serde_json::Value =
            serde_json::from_str(&cleaned).expect("should be valid JSON");
        assert_eq!(parsed["summary"], "A test");
    }

    #[test]
    fn clean_json_handles_trailing_thoughts() {
        let s = r#"{"summary": "A test", "description": null, "license_suggestion": null, "confidence": 0.9, "notes": []} and then some more thoughts about the package..."#;
        let cleaned = clean_llm_json(s).expect("should extract JSON");
        let parsed: serde_json::Value =
            serde_json::from_str(&cleaned).expect("should be valid JSON");
        assert_eq!(parsed["summary"], "A test");
    }

    #[test]
    fn clean_json_returns_error_on_no_json() {
        let s = "No JSON here, just random text.";
        assert!(clean_llm_json(s).is_err());
    }
}
