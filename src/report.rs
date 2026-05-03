use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    core::{BuildAction, BuildIssue},
    utils::llm::TextSuggestion,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    BuildSuccess,
    NeedHuman,
    Failed,
    DryRunComplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedFix {
    pub action: BuildAction,
    pub diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub status: Status,
    pub package_name: Option<String>,
    pub spec_path: Option<PathBuf>,
    pub build_attempts: usize,
    pub fixes_applied: usize,
    pub last_log_path: Option<PathBuf>,
    pub operation_log_path: Option<PathBuf>,
    pub final_issue: Option<BuildIssue>,
    pub final_action: Option<BuildAction>,
    pub llm_text_suggestion: Option<TextSuggestion>,
    pub applied_fixes: Vec<AppliedFix>,
    pub notes: Vec<String>,
}

impl Report {
    pub fn new(status: Status) -> Self {
        Self {
            status,
            package_name: None,
            spec_path: None,
            build_attempts: 0,
            fixes_applied: 0,
            last_log_path: None,
            operation_log_path: None,
            final_issue: None,
            final_action: None,
            llm_text_suggestion: None,
            applied_fixes: Vec::new(),
            notes: Vec::new(),
        }
    }
}

pub fn print_report(report: &Report, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }

    if let Some(pkg) = &report.package_name {
        println!("package: {pkg}");
    }
    println!("status: {:?}", report.status);
    if let Some(spec_path) = &report.spec_path {
        println!("spec: {}", spec_path.display());
    }
    println!("build_attempts: {}", report.build_attempts);
    println!("fixes_applied: {}", report.fixes_applied);
    if let Some(log) = &report.last_log_path {
        println!("last_log: {}", log.display());
    }
    if let Some(log) = &report.operation_log_path {
        println!("operations_log: {}", log.display());
    }
    if let Some(action) = &report.final_action {
        println!("final_action: {action:#?}");
    }
    for note in &report.notes {
        println!("note: {note}");
    }
    Ok(())
}

pub fn print_reports(reports: &[Report], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(reports)?);
        return Ok(());
    }

    for (idx, report) in reports.iter().enumerate() {
        if idx > 0 {
            println!();
        }
        print_report(report, false)?;
    }
    Ok(())
}
