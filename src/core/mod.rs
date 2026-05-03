pub mod engine;
pub mod graph;
pub mod resources;
pub mod scheduler;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildIssue {
    MissingBuildDependencies {
        deps: Vec<String>,
    },
    DependencyUnresolvable {
        deps: Vec<String>,
    },
    MissingPythonModule {
        module: String,
        import_context: Option<String>,
    },
    ImportCheckExclusions {
        modules: Vec<String>,
        missing_modules: Vec<String>,
        exclusions: Vec<String>,
    },
    EmptyImportCheck,
    InstalledButUnpackagedFiles {
        files: Vec<String>,
    },
    ArchDependentInNoarch,
    MissingPep639LicenseMetadata,
    InstallModuleMismatch {
        wrong_module: String,
        suggested_module: String,
    },
    PyprojectBackendMissing {
        deps: Vec<String>,
    },
    CExtensionCompileError {
        important_lines: Vec<String>,
    },
    TestFailure {
        important_lines: Vec<String>,
    },
    PatchApplyError {
        important_lines: Vec<String>,
    },
    Unknown {
        important_lines: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildAction {
    AddBuildRequires {
        deps: Vec<String>,
        reason: String,
    },
    AddBuildOptionCheckExcludes {
        patterns: Vec<String>,
        reason: String,
    },
    AddEmptyCheckSection {
        comment: String,
        reason: String,
    },
    FixBuildOptionInstall {
        arg: String,
        reason: String,
    },
    AppendFilesEntries {
        files: Vec<String>,
        reason: String,
    },
    RemoveNoarch {
        reason: String,
    },
    NeedHuman {
        reason: String,
        issue: BuildIssue,
    },
    NoOp {
        reason: String,
    },
}

impl BuildAction {
    pub fn is_need_human(&self) -> bool {
        matches!(self, Self::NeedHuman { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisOutput {
    pub issue: BuildIssue,
    pub action: BuildAction,
}
