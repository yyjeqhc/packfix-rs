use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Clone, Parser)]
#[command(name = "packfix", about = "Deterministic Python RPM build fixer")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    #[arg(long, global = true)]
    pub project: Option<String>,

    #[arg(long, default_value = "x64", global = true)]
    pub repository: String,

    #[arg(long, default_value = "x86_64", global = true)]
    pub arch: String,

    #[arg(long, default_value_t = 2, global = true)]
    pub max_retries: usize,

    #[arg(long, default_value_t = 3, global = true)]
    pub max_dep_depth: usize,

    #[arg(long, default_value = "takopack", global = true)]
    pub takopack_bin: PathBuf,

    #[arg(long, default_value = "osc", global = true)]
    pub osc_bin: PathBuf,

    #[arg(long, default_value = "http://localhost", global = true)]
    pub ollama_host: String,

    #[arg(long, default_value_t = 11434, global = true)]
    pub ollama_port: u16,

    #[arg(long, default_value = "qwen3:8b", global = true)]
    pub model: String,

    #[arg(long, global = true)]
    pub apply_text: bool,

    #[arg(long, global = true)]
    pub json: bool,

    #[arg(long, global = true)]
    pub obs_api_url: Option<String>,

    #[arg(long, global = true)]
    pub repo_url: Option<String>,

    #[arg(long, global = true)]
    pub oscrc_path: Option<PathBuf>,

    #[arg(long, global = true)]
    pub default_obs_project: Option<String>,

    /// Auto-generate and replace %description via LLM before building
    #[arg(long, global = true, default_value_t = true)]
    pub llm_description: bool,

    /// Disable LLM-based %description generation (overrides --llm-description)
    #[arg(long, global = true)]
    pub no_llm_description: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Build a Python package: takopack -> git -> EBF -> osc build -> fix -> retry
    Build {
        pypi_names: Vec<String>,
        #[arg(long)]
        version: Option<String>,
        #[arg(long = "obs-project")]
        obs_project: Option<String>,
        #[arg(long)]
        revision: Option<String>,
    },

    /// Build existing packages already present in the repo: git -> EBF -> osc build -> fix -> retry
    BuildExisting {
        #[arg(required = true)]
        packages: Vec<String>,
        #[arg(long = "obs-project")]
        obs_project: Option<String>,
        #[arg(long)]
        revision: Option<String>,
    },

    /// Analyze a build log
    AnalyzeLog { log_file: PathBuf },

    /// Dry-run local build + fix
    DryRun { workdir: PathBuf },

    /// Local build + fix loop (one or more workdirs)
    Fix {
        #[arg(required = true)]
        workdirs: Vec<PathBuf>,
    },

    /// Generate new spec with takopack
    New {
        pypi_name: String,
        version: Option<String>,
    },

    /// LLM Summary/Description suggestions
    Summarize { workdir: PathBuf },

    /// Show effective configuration
    ConfigShow,

    /// osc checkout a package from OBS
    Checkout {
        project: String,
        #[arg(long)]
        package: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// Update a repo package by RPM name, or run osc up -S when given a workdir path
    Update {
        target: String,
        #[arg(long = "obs-project")]
        obs_project: Option<String>,
    },

    /// osc api build status
    Status {
        project: String,
        package: String,
        #[arg(long)]
        repository: Option<String>,
        #[arg(long)]
        arch: Option<String>,
    },

    /// Push packages to OBS via EBF API
    Submit {
        revision: String,
        #[arg(required = true)]
        components: Vec<String>,
        #[arg(long = "obs-project")]
        obs_project: Option<String>,
    },

    /// Generate an RPM spec %description from a source directory via LLM
    Describe {
        dir: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn llm_description_defaults_to_true() {
        let cli = Cli::try_parse_from(["packfix", "config-show"]).unwrap();
        assert!(cli.llm_description);
        assert!(!cli.no_llm_description);
    }

    #[test]
    fn no_llm_description_flag_disables() {
        let cli = Cli::try_parse_from(["packfix", "--no-llm-description", "config-show"]).unwrap();
        // llm_description stays at its default (true); the application code
        // checks no_llm_description to override it.
        assert!(cli.llm_description);
        assert!(cli.no_llm_description);
    }

    #[test]
    fn both_flags_can_coexist() {
        let cli = Cli::try_parse_from([
            "packfix",
            "--llm-description",
            "--no-llm-description",
            "config-show",
        ])
        .unwrap();
        assert!(cli.llm_description);
        assert!(cli.no_llm_description);
    }
}
