use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

fn default_description_system_prompt() -> String {
    "You generate RPM spec file %description bodies for Python packages. Use only the provided package metadata, README excerpt, and module list. Do not invent unsupported facts. Output plain English text only. Do not output Markdown, bullets, headings, quotes, or the '%description' tag. Write in a concise downstream packaging style, not in marketing style.".into()
}

fn default_description_user_prompt() -> String {
    "Task: Generate RPM spec %description body.\n\nExtracted package information:\n{context}\n\nHard requirements:\n- English only.\n- Output 1 to 5 lines.\n- Each line must be no longer than 100 characters.\n- Plain text only.\n- Do not include \"%description\".\n- Do not include a package name/version heading.\n- Do not mention sources, metadata, README, uncertainty, classifiers, or license.\n- Do not mention Python version requirements unless essential to the package purpose.\n- Avoid generic filler such as \"various applications\", \"used by developers\",\n  \"advanced customization\", \"provides utilities\", or \"secure protocols\".\n- Prefer concrete capabilities over broad claims.\n- Describe what the package provides and what it is used for.\n- The result should be suitable for direct insertion under an RPM spec %description.".into()
}

const fn default_description_timeout_secs() -> u64 {
    180
}

const fn default_description_max_context_chars() -> usize {
    12000
}

const fn default_description_num_predict() -> i32 {
    160
}

fn default_description_temperature() -> f32 {
    0.0
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PackfixConfig {
    pub obs: Option<ObsConfig>,
    pub repo: Option<RepoConfig>,
    pub llm: Option<LlmConfig>,
    pub build: Option<BuildConfig>,
    #[serde(default)]
    pub description: Option<DescriptionConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObsConfig {
    pub api_url: Option<String>,
    pub default_project: Option<String>,
    pub oscrc_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoConfig {
    pub url: Option<String>,
    pub workdir: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildConfig {
    pub repository: Option<String>,
    pub arch: Option<String>,
    pub max_retries: Option<usize>,
    pub max_dep_depth: Option<usize>,
    pub llm_description: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DescriptionConfig {
    #[serde(default = "default_description_system_prompt")]
    pub system_prompt: String,
    #[serde(default = "default_description_user_prompt")]
    pub user_prompt: String,
    #[serde(default = "default_description_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_description_max_context_chars")]
    pub max_context_chars: usize,
    #[serde(default = "default_description_num_predict")]
    pub num_predict: i32,
    #[serde(default = "default_description_temperature")]
    pub temperature: f32,
}

pub fn load_config() -> Result<Option<PackfixConfig>> {
    for path in config_paths() {
        if !path.exists() {
            continue;
        }
        return load_from_file(&path).map(Some);
    }
    Ok(None)
}

fn load_from_file(path: &Path) -> Result<PackfixConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&content)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

pub fn config_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("packfix.toml")];
    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(home).join(".config/packfix/config.toml"));
    }
    paths
}

pub fn resolve_obs_api_url(
    cli_obs_api_url: Option<&str>,
    config: Option<&PackfixConfig>,
) -> String {
    cli_obs_api_url
        .map(String::from)
        .or_else(|| {
            config
                .and_then(|c| c.obs.as_ref())
                .and_then(|o| o.api_url.clone())
        })
        .unwrap_or_else(|| "https://pickaxe.oerv.ac.cn/".to_string())
}

pub fn resolve_repo_url(cli_repo_url: Option<&str>, config: Option<&PackfixConfig>) -> String {
    cli_repo_url
        .map(String::from)
        .or_else(|| {
            config
                .and_then(|c| c.repo.as_ref())
                .and_then(|r| r.url.clone())
        })
        .unwrap_or_else(|| "https://github.com/yyjeqhc/openruyi/".to_string())
}

pub fn resolve_default_obs_project(
    cli_default_obs_project: Option<&str>,
    config: Option<&PackfixConfig>,
) -> String {
    cli_default_obs_project
        .map(String::from)
        .or_else(|| {
            config
                .and_then(|c| c.obs.as_ref())
                .and_then(|o| o.default_project.clone())
        })
        .unwrap_or_else(|| "home:yyjeqhc:new_ruyios".to_string())
}

pub fn resolve_oscrc_path(
    cli_oscrc_path: Option<&PathBuf>,
    config: Option<&PackfixConfig>,
) -> PathBuf {
    cli_oscrc_path
        .cloned()
        .or_else(|| {
            config
                .and_then(|c| c.obs.as_ref())
                .and_then(|o| o.oscrc_path.clone())
        })
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
            PathBuf::from(home).join(".config/osc/oscrc")
        })
}

pub fn resolve_description_system_prompt(config: Option<&PackfixConfig>) -> String {
    config
        .and_then(|c| c.description.as_ref())
        .map(|d| d.system_prompt.clone())
        .unwrap_or_else(default_description_system_prompt)
}

pub fn resolve_description_user_prompt(config: Option<&PackfixConfig>) -> String {
    config
        .and_then(|c| c.description.as_ref())
        .map(|d| d.user_prompt.clone())
        .unwrap_or_else(default_description_user_prompt)
}

pub fn resolve_description_timeout_secs(config: Option<&PackfixConfig>) -> u64 {
    config
        .and_then(|c| c.description.as_ref())
        .map(|d| d.timeout_secs)
        .unwrap_or_else(default_description_timeout_secs)
}

pub fn resolve_description_max_context_chars(config: Option<&PackfixConfig>) -> usize {
    config
        .and_then(|c| c.description.as_ref())
        .map(|d| d.max_context_chars)
        .unwrap_or_else(default_description_max_context_chars)
}

pub fn resolve_description_num_predict(config: Option<&PackfixConfig>) -> i32 {
    config
        .and_then(|c| c.description.as_ref())
        .map(|d| d.num_predict)
        .unwrap_or_else(default_description_num_predict)
}

pub fn resolve_description_temperature(config: Option<&PackfixConfig>) -> f32 {
    config
        .and_then(|c| c.description.as_ref())
        .map(|d| d.temperature)
        .unwrap_or_else(default_description_temperature)
}

pub fn resolve_repo_dir(config: Option<&PackfixConfig>) -> PathBuf {
    config
        .and_then(|c| c.repo.as_ref())
        .and_then(|r| r.workdir.clone())
        .unwrap_or_else(|| PathBuf::from("/root/git/openruyi-repo"))
}

pub fn display_config(config: Option<&PackfixConfig>, json: bool) -> Result<()> {
    if json {
        #[derive(serde::Serialize)]
        struct DisplayConfig {
            obs_api_url: String,
            repo_url: String,
            default_obs_project: String,
            oscrc_path: String,
            ollama_host: String,
            ollama_port: u16,
            model: String,
            description_system_prompt: String,
            description_user_prompt: String,
            description_timeout_secs: u64,
            description_max_context_chars: usize,
            description_num_predict: i32,
            description_temperature: f32,
            build_repository: String,
            build_arch: String,
        }
        let dc = DisplayConfig {
            obs_api_url: resolve_obs_api_url(None, config),
            repo_url: resolve_repo_url(None, config),
            default_obs_project: resolve_default_obs_project(None, config),
            oscrc_path: resolve_oscrc_path(None, config).display().to_string(),
            ollama_host: config
                .and_then(|c| c.llm.as_ref())
                .and_then(|l| l.host.clone())
                .unwrap_or_else(|| "http://localhost".to_string()),
            ollama_port: config
                .and_then(|c| c.llm.as_ref())
                .and_then(|l| l.port)
                .unwrap_or(11434),
            model: config
                .and_then(|c| c.llm.as_ref())
                .and_then(|l| l.model.clone())
                .unwrap_or_else(|| "qwen3:8b".to_string()),
            build_repository: config
                .and_then(|c| c.build.as_ref())
                .and_then(|b| b.repository.clone())
                .unwrap_or_else(|| "x64".to_string()),
            build_arch: config
                .and_then(|c| c.build.as_ref())
                .and_then(|b| b.arch.clone())
                .unwrap_or_else(|| "x86_64".to_string()),
            description_system_prompt: resolve_description_system_prompt(config),
            description_user_prompt: resolve_description_user_prompt(config),
            description_timeout_secs: resolve_description_timeout_secs(config),
            description_max_context_chars: resolve_description_max_context_chars(config),
            description_num_predict: resolve_description_num_predict(config),
            description_temperature: resolve_description_temperature(config),
        };
        println!("{}", serde_json::to_string_pretty(&dc)?);
    } else {
        if let Some(cfg) = config {
            if let Some(obs) = &cfg.obs {
                if let Some(v) = &obs.api_url {
                    println!("obs.api_url = {v:?}");
                }
                if let Some(v) = &obs.default_project {
                    println!("obs.default_project = {v:?}");
                }
            }
            if let Some(repo) = &cfg.repo
                && let Some(v) = &repo.url
            {
                println!("repo.url = {v:?}");
            }
            if let Some(llm) = &cfg.llm {
                if let Some(v) = &llm.host {
                    println!("llm.host = {v:?}");
                }
                if let Some(v) = llm.port {
                    println!("llm.port = {v}");
                }
                if let Some(v) = &llm.model {
                    println!("llm.model = {v:?}");
                }
            }
            if let Some(build) = &cfg.build {
                if let Some(v) = &build.repository {
                    println!("build.repository = {v:?}");
                }
                if let Some(v) = &build.arch {
                    println!("build.arch = {v:?}");
                }
                if let Some(v) = build.max_retries {
                    println!("build.max_retries = {v}");
                }
                if let Some(v) = build.max_dep_depth {
                    println!("build.max_dep_depth = {v}");
                }
            }
        }
        println!("---resolved---");
        println!(
            "description_system_prompt = {:?}",
            resolve_description_system_prompt(config)
        );
        println!(
            "description_user_prompt = {:?}",
            resolve_description_user_prompt(config)
        );
        println!(
            "description_timeout_secs = {}",
            resolve_description_timeout_secs(config)
        );
        println!(
            "description_max_context_chars = {}",
            resolve_description_max_context_chars(config)
        );
        println!(
            "description_num_predict = {}",
            resolve_description_num_predict(config)
        );
        println!(
            "description_temperature = {}",
            resolve_description_temperature(config)
        );
        println!("obs_api_url = {}", resolve_obs_api_url(None, config));
        println!("repo_url = {}", resolve_repo_url(None, config));
        println!(
            "default_obs_project = {}",
            resolve_default_obs_project(None, config)
        );
        println!(
            "oscrc_path = {}",
            resolve_oscrc_path(None, config).display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml = r#"
[obs]
api_url = "https://example.com/"
default_project = "home:test:proj"
oscrc_path = "/home/test/.config/osc/oscrc"

[repo]
url = "https://github.com/test/repo"

[llm]
host = "http://ollama:11434"
port = 9999
model = "test-model"

[build]
repository = "rv64"
arch = "riscv64"
max_retries = 3
"#;
        let config: PackfixConfig = toml::from_str(toml).expect("should parse");
        assert_eq!(
            config.obs.as_ref().unwrap().api_url,
            Some("https://example.com/".into())
        );
        assert_eq!(
            config.repo.as_ref().unwrap().url,
            Some("https://github.com/test/repo".into())
        );
        assert_eq!(config.llm.as_ref().unwrap().port, Some(9999));
        assert_eq!(config.build.as_ref().unwrap().arch, Some("riscv64".into()));
    }

    #[test]
    fn parse_empty_config() {
        let config: PackfixConfig = toml::from_str("").expect("should parse empty");
        assert!(config.obs.is_none());
        assert!(config.repo.is_none());
    }

    #[test]
    fn parse_partial_config() {
        let toml = r#"
[obs]
api_url = "https://example.com/"
"#;
        let config: PackfixConfig = toml::from_str(toml).expect("should parse");
        assert!(config.obs.is_some());
        assert!(config.repo.is_none());
    }

    #[test]
    fn resolve_obs_api_url_cli_wins() {
        let url = resolve_obs_api_url(Some("https://cli.example.com"), None);
        assert_eq!(url, "https://cli.example.com");
    }

    #[test]
    fn resolve_obs_api_url_config_fallback() {
        let toml = r#"[obs]
api_url = "https://config.example.com"
"#;
        let config: PackfixConfig = toml::from_str(toml).unwrap();
        let url = resolve_obs_api_url(None, Some(&config));
        assert_eq!(url, "https://config.example.com");
    }

    #[test]
    fn resolve_repo_url_default() {
        let url = resolve_repo_url(None, None);
        assert!(url.contains("github.com"));
    }

    #[test]
    fn description_system_prompt_default() {
        let prompt = resolve_description_system_prompt(None);
        assert!(prompt.contains("RPM spec file %description"));
    }

    #[test]
    fn description_user_prompt_default() {
        let prompt = resolve_description_user_prompt(None);
        assert!(prompt.contains("{context}"));
        assert!(prompt.contains("1 to 5 lines"));
    }

    #[test]
    fn description_prompt_from_config() {
        let toml = r#"
[description]
system_prompt = "Custom system"
user_prompt = "Custom user {context}"
"#;
        let config: PackfixConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            resolve_description_system_prompt(Some(&config)),
            "Custom system"
        );
        assert_eq!(
            resolve_description_user_prompt(Some(&config)),
            "Custom user {context}"
        );
    }

    #[test]
    fn description_prompt_default_when_section_missing() {
        let toml = r#"
[obs]
api_url = "https://example.com/"
"#;
        let config: PackfixConfig = toml::from_str(toml).unwrap();
        let prompt = resolve_description_system_prompt(Some(&config));
        assert!(prompt.contains("RPM spec file %description"));
    }

    #[test]
    fn description_timeout_secs_default() {
        let secs = resolve_description_timeout_secs(None);
        assert_eq!(secs, 180);
    }

    #[test]
    fn description_timeout_secs_from_config() {
        let toml = r#"
[description]
timeout_secs = 60
"#;
        let config: PackfixConfig = toml::from_str(toml).unwrap();
        let secs = resolve_description_timeout_secs(Some(&config));
        assert_eq!(secs, 60);
    }

    #[test]
    fn description_max_context_chars_default() {
        assert_eq!(resolve_description_max_context_chars(None), 12000);
    }

    #[test]
    fn description_num_predict_default() {
        assert_eq!(resolve_description_num_predict(None), 160);
    }

    #[test]
    fn description_temperature_default() {
        assert!((resolve_description_temperature(None) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn load_config_returns_none_when_no_files_exist() {
        // load_config checks cwd for packfix.toml and ~/.config/packfix/config.toml.
        // In a temp dir neither exists, so it should return Ok(None).
        let dir = tempfile::tempdir().unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = load_config();
        std::env::set_current_dir(original).unwrap();
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn load_from_file_parses_valid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("packfix.toml");
        std::fs::write(&path, "[obs]\napi_url = \"https://test.example.com\"\n").unwrap();
        let config = load_from_file(&path).unwrap();
        assert_eq!(
            config.obs.unwrap().api_url.unwrap(),
            "https://test.example.com"
        );
    }

    #[test]
    fn load_from_file_errors_on_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("packfix.toml");
        std::fs::write(&path, "this is not valid toml [[[[").unwrap();
        let err = load_from_file(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to parse config file"), "got: {msg}");
        assert!(
            msg.contains("packfix.toml"),
            "error must include path, got: {msg}"
        );
    }

    #[test]
    fn load_from_file_errors_on_read_failure() {
        let path = std::path::Path::new("/nonexistent/path/packfix.toml");
        let err = load_from_file(path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to read config file"), "got: {msg}");
    }

    #[test]
    fn load_config_errors_on_invalid_toml_in_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("packfix.toml"), "bad [[[").unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = load_config();
        std::env::set_current_dir(original).unwrap();
        let err = result.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to parse"), "got: {msg}");
    }
}
