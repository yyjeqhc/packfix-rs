pub mod analyzer;
pub mod fixer;

use std::path::Path;

use crate::core::BuildIssue;
use crate::upstream;

/// Refine a [`BuildIssue`] by inspecting the source tree in `workdir`.
///
/// Currently only `InstallModuleMismatch` benefits: the analyzer initially
/// sets `suggested_module == wrong_module`; this function uses the source
/// archive layout to infer the correct module name.
pub fn contextualize_issue(workdir: &Path, issue: BuildIssue) -> anyhow::Result<BuildIssue> {
    match issue {
        BuildIssue::InstallModuleMismatch {
            wrong_module,
            suggested_module,
        } => {
            if let Some(inferred) = upstream::infer_install_module(workdir, &wrong_module)?
                && inferred != suggested_module
            {
                return Ok(BuildIssue::InstallModuleMismatch {
                    wrong_module,
                    suggested_module: inferred,
                });
            }
            Ok(BuildIssue::InstallModuleMismatch {
                wrong_module,
                suggested_module,
            })
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn contextualize_issue_passes_through_non_mismatch() {
        let dir = tempdir().unwrap();
        let issue = BuildIssue::MissingBuildDependencies {
            deps: vec!["python3dist(foo)".into()],
        };
        let result = contextualize_issue(dir.path(), issue.clone()).unwrap();
        assert_eq!(result, issue);
    }

    #[test]
    fn contextualize_issue_preserves_mismatch_when_no_source() {
        let dir = tempdir().unwrap();
        let issue = BuildIssue::InstallModuleMismatch {
            wrong_module: "zope_interface".into(),
            suggested_module: "zope_interface".into(),
        };
        let result = contextualize_issue(dir.path(), issue).unwrap();
        match result {
            BuildIssue::InstallModuleMismatch {
                wrong_module,
                suggested_module,
            } => {
                assert_eq!(wrong_module, "zope_interface");
                assert_eq!(suggested_module, "zope_interface");
            }
            other => panic!("expected InstallModuleMismatch, got {other:?}"),
        }
    }

    #[test]
    fn contextualize_issue_refines_module_from_egg_info() {
        let dir = tempdir().unwrap();
        // Create a minimal source tree: <dir>/mypkg.egg-info/top_level.txt
        let egg_dir = dir.path().join("mypkg-1.0.egg-info");
        std::fs::create_dir(&egg_dir).unwrap();
        std::fs::write(egg_dir.join("top_level.txt"), "real_module\n").unwrap();

        let issue = BuildIssue::InstallModuleMismatch {
            wrong_module: "wrong_name".into(),
            suggested_module: "wrong_name".into(),
        };
        let result = contextualize_issue(dir.path(), issue).unwrap();
        match result {
            BuildIssue::InstallModuleMismatch {
                wrong_module,
                suggested_module,
            } => {
                assert_eq!(wrong_module, "wrong_name");
                assert_eq!(
                    suggested_module, "real_module",
                    "should infer real_module from egg-info/top_level.txt"
                );
            }
            other => panic!("expected InstallModuleMismatch, got {other:?}"),
        }
    }
}
