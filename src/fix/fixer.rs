use anyhow::Result;

use crate::{
    core::{BuildAction, BuildIssue},
    spec,
};

pub fn decide_action(issue: &BuildIssue) -> BuildAction {
    match issue {
        BuildIssue::MissingBuildDependencies { deps } => BuildAction::AddBuildRequires {
            deps: deps.clone(),
            reason: "Build log listed missing build dependencies.".into(),
        },
        BuildIssue::DependencyUnresolvable { .. } => BuildAction::NeedHuman {
            reason: "Dependency is already in solver resolution but unavailable in the repository; package the dependency or adjust repositories first.".into(),
            issue: issue.clone(),
        },
        BuildIssue::ImportCheckExclusions { exclusions, .. } => {
            BuildAction::AddBuildOptionCheckExcludes {
                patterns: exclusions.clone(),
                reason: "Grouped import_all_modules failures should be excluded from BuildOption(check) instead of being treated as ordinary missing modules.".into(),
            }
        }
        BuildIssue::EmptyImportCheck => BuildAction::AddEmptyCheckSection {
            comment: "No importable runtime modules for default import check.".into(),
            reason: "Default import_all_modules check had no modules left to verify.".into(),
        },
        BuildIssue::InstalledButUnpackagedFiles { files } => BuildAction::AppendFilesEntries {
            files: files.clone(),
            reason: "Build log listed installed but unpackaged files.".into(),
        },
        BuildIssue::ArchDependentInNoarch => BuildAction::RemoveNoarch {
            reason: "Build produced arch-dependent binaries in a noarch package.".into(),
        },
        BuildIssue::MissingPep639LicenseMetadata => BuildAction::FixBuildOptionInstall {
            arg: "-L".into(),
            reason: "PEP 639 license metadata is missing; add -L to BuildOption(install).".into(),
        },
        BuildIssue::InstallModuleMismatch { suggested_module, .. } => {
            BuildAction::FixBuildOptionInstall {
                arg: suggested_module.clone(),
                reason: "BuildOption(install) module glob does not match installed module.".into(),
            }
        }
        BuildIssue::MissingPythonModule { .. }
        | BuildIssue::PyprojectBackendMissing { .. }
        | BuildIssue::CExtensionCompileError { .. }
        | BuildIssue::TestFailure { .. }
        | BuildIssue::PatchApplyError { .. }
        | BuildIssue::Unknown { .. } => BuildAction::NeedHuman {
            reason: "No safe deterministic fix is available.".into(),
            issue: issue.clone(),
        },
    }
}

pub fn apply_action(spec_text: &str, action: &BuildAction) -> Result<String> {
    let new_text = match action {
        BuildAction::AddBuildRequires { deps, .. } => spec::add_buildrequires(spec_text, deps),
        BuildAction::AddBuildOptionCheckExcludes { patterns, .. } => {
            spec::add_buildoption_checks(spec_text, patterns)
        }
        BuildAction::AddEmptyCheckSection { comment, .. } => {
            spec::add_empty_check_section(spec_text, comment)
        }
        BuildAction::FixBuildOptionInstall { arg, .. } => {
            spec::fix_buildoption_install(spec_text, arg)
        }
        BuildAction::AppendFilesEntries { files, .. } => {
            spec::append_files_entries(spec_text, files)
        }
        BuildAction::RemoveNoarch { .. } => spec::fix_buildarch_remove_noarch(spec_text),
        BuildAction::NoOp { .. } | BuildAction::NeedHuman { .. } => spec_text.to_string(),
    };
    Ok(new_text)
}
