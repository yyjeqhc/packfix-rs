use regex::Regex;

use crate::core::BuildIssue;

pub fn analyze_log(log: &str) -> BuildIssue {
    if let Some(issue) = install_module_mismatch(log) {
        return issue;
    }
    if empty_import_check(log) {
        return BuildIssue::EmptyImportCheck;
    }
    if let Some(issue) = import_check_exclusion_issue(log) {
        return issue;
    }
    if let Some(deps) = failed_build_deps(log) {
        return BuildIssue::MissingBuildDependencies { deps };
    }
    if let Some(deps) = unresolvable_deps(log) {
        return BuildIssue::DependencyUnresolvable { deps };
    }
    if let Some(files) = unpackaged_files(log) {
        return BuildIssue::InstalledButUnpackagedFiles { files };
    }
    if log.contains("Arch dependent binaries in noarch package") {
        return BuildIssue::ArchDependentInNoarch;
    }
    if log.contains("No License-File (PEP 639)") {
        return BuildIssue::MissingPep639LicenseMetadata;
    }
    if is_c_extension_error(log) {
        return BuildIssue::CExtensionCompileError {
            important_lines: important_lines(log),
        };
    }
    if is_patch_error(log) {
        return BuildIssue::PatchApplyError {
            important_lines: important_lines(log),
        };
    }
    if is_test_failure(log) {
        return BuildIssue::TestFailure {
            important_lines: important_lines(log),
        };
    }
    if let Some(module) = missing_python_module(log) {
        return BuildIssue::MissingPythonModule {
            module,
            import_context: None,
        };
    }
    BuildIssue::Unknown {
        important_lines: important_lines(log),
    }
}

fn failed_build_deps(log: &str) -> Option<Vec<String>> {
    if !log.contains("Failed build dependencies:") {
        return None;
    }
    let dep_re = Regex::new(r"(python3dist\([^)]+\)|pkgconfig\([^)]+\))").expect("valid regex");
    let mut deps = Vec::new();
    let mut in_block = false;
    for line in log.lines() {
        if line.contains("Failed build dependencies:") {
            in_block = true;
            continue;
        }
        if in_block {
            if line.trim().is_empty() {
                break;
            }
            if let Some(cap) = dep_re.captures(line) {
                push_unique(&mut deps, cap[1].to_string());
            }
        }
    }
    (!deps.is_empty()).then_some(deps)
}

fn unresolvable_deps(log: &str) -> Option<Vec<String>> {
    let dep_re = Regex::new(
        r"(?:nothing provides|unresolvable:\s*nothing provides)\s+((?:python[\d.]*dist|pkgconfig)\([^)]+\))",
    )
    .expect("valid regex");
    let mut deps = Vec::new();
    for cap in dep_re.captures_iter(log) {
        push_unique(&mut deps, cap[1].to_string());
    }
    (!deps.is_empty()).then_some(deps)
}

fn unpackaged_files(log: &str) -> Option<Vec<String>> {
    // 这里按用户要求：精确匹配固定错误字符串
    if !(log.contains("Installed (but unpackaged) file(s) found")
        || log.contains("Installed (but unpackaged) file(s) found:")
        || log.contains("Installed but unpackaged files found")
        || log.contains("Installed but unpackaged files found:"))
    {
        return None;
    }
    let mut files = Vec::new();
    let mut in_block = false;
    let path_re = Regex::new(r"(/[^\s]+)").expect("valid regex");
    for line in log.lines() {
        let trimmed = line.trim();
        if !in_block {
            if trimmed.contains("Installed (but unpackaged) file(s) found")
                || trimmed.contains("Installed but unpackaged files found")
            {
                in_block = true;
                continue;
            }
        } else {
            if let Some(cap) = path_re.captures(line) {
                let path = cap[1].to_string();
                push_unique(&mut files, path);
                continue;
            }
            if trimmed.is_empty() && !files.is_empty() {
                break;
            }
        }
    }
    (!files.is_empty()).then_some(files)
}

fn install_module_mismatch(log: &str) -> Option<BuildIssue> {
    let re = Regex::new(
        r#"(?m)^(?:\[[^\]]+\]\s*)?(?:ValueError:\s+)?Globs did not match any module:\s*(\S+)\s*$"#,
    )
    .expect("valid regex");
    let wrong = re.captures(log)?.get(1)?.as_str().to_string();
    let suggested = wrong.clone();
    Some(BuildIssue::InstallModuleMismatch {
        wrong_module: wrong,
        suggested_module: suggested,
    })
}

fn empty_import_check(log: &str) -> bool {
    log.contains("import_all_modules.py")
        && log.contains("ValueError: No modules to check were left")
}

fn import_check_exclusion_issue(log: &str) -> Option<BuildIssue> {
    if !log.contains("import_all_modules.py") {
        return None;
    }
    let import_re = Regex::new(r"Check import:\s*(\S+)").expect("valid regex");
    let missing_re = Regex::new(r"No module named '([^']+)'").expect("valid regex");
    let has_import_failure = log.contains("ModuleNotFoundError")
        || log.contains("ImportError")
        || log.contains("is not installed")
        || log.contains("Please install it with")
        || log.contains("pip install");
    if !has_import_failure {
        return None;
    }

    // 优先使用 "Failed to import:" 行中的明确列表
    // 先尝试显式的 `Failed to import:` 行（这是最可靠的来源）
    let (modules, explicit) = if let Some(mods) = failed_import_modules(log) {
        (mods, true)
    } else {
        // 备选方案：从 `Check import:` 日志行推断，但只保留那些被认为是安全可排除的项（旧逻辑）。
        let grouped: Vec<String> = import_re
            .captures_iter(log)
            .map(|cap| cap[1].to_string())
            .filter(|module| safe_import_exclusion(module).is_some())
            .collect();
        if grouped.is_empty() {
            return None;
        }
        (grouped, false)
    };

    let mut missing_modules = Vec::new();
    for cap in missing_re.captures_iter(log) {
        push_unique(&mut missing_modules, cap[1].to_string());
    }
    let exclusions = if explicit {
        explicit_exclusions(&modules)
    } else {
        grouped_exclusions_for(&modules)
    };
    if exclusions.is_empty() {
        return None;
    }
    Some(BuildIssue::ImportCheckExclusions {
        modules,
        missing_modules,
        exclusions,
    })
}

fn failed_import_modules(log: &str) -> Option<Vec<String>> {
    let failed_line = log
        .lines()
        .find(|line| line.contains("Failed to import:"))?;
    let (_, rest) = failed_line.split_once("Failed to import:")?;
    let modules: Vec<String> = rest
        .split(',')
        .map(str::trim)
        .filter(|module| !module.is_empty())
        .map(String::from)
        .collect();
    // 直接返回所有失败导入的模块，无需进一步过滤
    // TODO: 后续可以优化排除策略，例如只排除真正可选的模块
    (!modules.is_empty()).then_some(modules)
}

fn is_optional_module(module: &str) -> bool {
    [
        ".integrations.",
        ".integration.",
        ".plugins.",
        ".plugin.",
        ".extras.",
        ".optional.",
    ]
    .iter()
    .any(|needle| module.contains(needle))
}

fn grouped_exclusions_for(modules: &[String]) -> Vec<String> {
    // 将模块按 parent（去掉最后一段）分组，然后生成排除规则 `parent.*`。
    // 为了避免产生大量重复或细粒度项，若某个 root（第一段）下的 parent 数目较多，则退而生成 `root.*`。
    use std::collections::{BTreeMap, BTreeSet};

    let mut by_root: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for module in modules {
        let key_parent = module_parent(module)
            .map(|p| p.to_string())
            .unwrap_or_else(|| module.to_string());
        let root = key_parent
            .split('.')
            .next()
            .unwrap_or(&key_parent)
            .to_string();
        by_root.entry(root).or_default().insert(key_parent);
    }

    let mut exclusions = Vec::new();
    for (root, parents) in by_root {
        // 如果某个 root 下 parent 数量很多（阈值 3），则退而使用 root.*，减少条目
        if parents.len() >= 3 {
            push_unique(&mut exclusions, format!("{root}.*"));
            continue;
        }
        // 否则列出每个 parent.*
        for parent in parents {
            push_unique(&mut exclusions, format!("{parent}.*"));
        }
    }

    exclusions
}

// 当日志中存在明确的 `Failed to import:` 列表时，直接把这些具体模块名作为排除项（去重、稳定顺序）。
fn explicit_exclusions(modules: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut exclusions = Vec::new();
    for module in modules {
        let trimmed = module.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !seen.contains(trimmed) {
            seen.insert(trimmed.to_string());
            exclusions.push(trimmed.to_string());
        }
    }
    exclusions
}

// TODO: 后续优化排除策略时可能用到
// 当前策略是直接排除 Failed to import 中列出的所有模块
fn safe_import_exclusion(module: &str) -> Option<String> {
    // 测试模块（显然是可选的）
    if module.contains(".tests.")
        || last_segment(module).is_some_and(|seg| seg.starts_with("test_"))
    {
        return module_parent(module).map(|parent| format!("{parent}.*"));
    }

    // 已显式标记为可选的模块
    if is_optional_module(module) || module.contains(".multidb.") {
        return module_parent(module).map(|parent| format!("{parent}.*"));
    }

    None
}

fn module_parent(module: &str) -> Option<&str> {
    let idx = module.rfind('.')?;
    Some(&module[..idx])
}

fn last_segment(module: &str) -> Option<&str> {
    module.rsplit('.').next()
}

fn missing_python_module(log: &str) -> Option<String> {
    let re = Regex::new(r"No module named '([^']+)'").expect("valid regex");
    re.captures(log).map(|cap| cap[1].to_string())
}

fn is_c_extension_error(log: &str) -> bool {
    log.contains("fatal error:") || log.contains("gcc failed") || log.contains("pkg-config")
}

fn is_patch_error(log: &str) -> bool {
    (log.contains("hunk FAILED")) || (log.contains("patch") && log.contains("FAILED"))
}

fn is_test_failure(log: &str) -> bool {
    log.contains("pytest") && (log.contains("FAILED") || log.contains("failed"))
}

fn important_lines(log: &str) -> Vec<String> {
    log.lines()
        .filter(|line| {
            let lower = line.to_lowercase();
            lower.contains("error")
                || lower.contains("failed")
                || lower.contains("nothing provides")
                || lower.contains("unresolvable")
                || lower.contains("not an obs scm working copy")
        })
        .take(20)
        .map(|line| line.trim().to_string())
        .collect()
}

fn push_unique(items: &mut Vec<String>, item: String) {
    if !items.contains(&item) {
        items.push(item);
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use crate::{core::BuildAction, fix::fixer::decide_action};

    use super::*;

    #[test]
    fn failed_build_dependencies_to_add_buildrequires() {
        let log = "Failed build dependencies:\n    python3dist(pytest) is needed by python-foo\n    python3dist(hatchling) >= 1.0 is needed by python-foo\n";
        let issue = analyze_log(log);
        assert_eq!(
            issue,
            BuildIssue::MissingBuildDependencies {
                deps: vec![
                    "python3dist(pytest)".into(),
                    "python3dist(hatchling)".into()
                ]
            }
        );
        assert!(
            matches!(decide_action(&issue), BuildAction::AddBuildRequires { deps, .. } if deps.len() == 2)
        );
    }

    #[test]
    fn unresolvable_to_need_human() {
        let issue = analyze_log("nothing provides python3dist(torch)");
        assert_eq!(
            issue,
            BuildIssue::DependencyUnresolvable {
                deps: vec!["python3dist(torch)".into()]
            }
        );
        assert!(matches!(
            decide_action(&issue),
            BuildAction::NeedHuman { .. }
        ));
    }

    #[test]
    fn optional_integration_import_failure_to_buildoption_check() {
        let log = r#"/usr/lib/rpm/openruyi/import_all_modules.py
Check import: lmformatenforcer.integrations.exllamav2
ModuleNotFoundError: No module named 'torch'
ImportError: exllamav2 is not installed. Please install it with "pip install exllamav2"
Check import: lmformatenforcer.integrations.haystackv1
ModuleNotFoundError: No module named 'haystack'
ImportError: haystack is not installed. Please install it with "pip install farm-haystack"
"#;
        let issue = analyze_log(log);
        assert!(matches!(
            &issue,
            BuildIssue::ImportCheckExclusions { exclusions, .. }
                if exclusions == &vec!["lmformatenforcer.integrations.*".to_string()]
        ));
        assert!(matches!(
            decide_action(&issue),
            BuildAction::AddBuildOptionCheckExcludes { patterns, .. }
                if patterns == vec!["lmformatenforcer.integrations.*".to_string()]
        ));
    }

    #[test]
    fn empty_import_check_becomes_empty_check_issue() {
        let log = r#"/usr/lib/rpm/openruyi/import_all_modules.py
ValueError: No modules to check were left
"#;
        let issue = analyze_log(log);
        assert_eq!(issue, BuildIssue::EmptyImportCheck);
        assert!(matches!(
            decide_action(&issue),
            BuildAction::AddEmptyCheckSection { comment, .. }
            if comment == "No importable runtime modules for default import check."
        ));
    }

    #[test]
    fn unpackaged_files_to_append_files() {
        let issue = analyze_log(
            "Installed but unpackaged files found:\n   /usr/bin/foo\n   /usr/share/foo/data.json\n",
        );
        assert!(
            matches!(&issue, BuildIssue::InstalledButUnpackagedFiles { files } if files.len() == 2)
        );
        assert!(
            matches!(decide_action(&issue), BuildAction::AppendFilesEntries { files, .. } if files.len() == 2)
        );
    }

    #[test]
    fn arch_dependent_to_remove_noarch() {
        let issue = analyze_log("Arch dependent binaries in noarch package");
        assert_eq!(issue, BuildIssue::ArchDependentInNoarch);
        assert!(matches!(
            decide_action(&issue),
            BuildAction::RemoveNoarch { .. }
        ));
    }

    #[test]
    fn pep639_to_buildoption_install_l() {
        let issue = analyze_log("No License-File (PEP 639) in upstream metadata found.");
        assert_eq!(issue, BuildIssue::MissingPep639LicenseMetadata);
        assert!(
            matches!(decide_action(&issue), BuildAction::FixBuildOptionInstall { arg, .. } if arg == "-L")
        );
    }

    #[test]
    fn install_module_mismatch() {
        let issue = analyze_log("Globs did not match any module: zope_interface");
        assert_eq!(
            issue,
            BuildIssue::InstallModuleMismatch {
                wrong_module: "zope_interface".into(),
                suggested_module: "zope_interface".into()
            }
        );
        assert!(
            matches!(decide_action(&issue), BuildAction::FixBuildOptionInstall { arg, .. } if arg == "zope_interface")
        );
    }

    #[test]
    fn install_module_mismatch_ignores_traceback_source_line() {
        let log = r#"
  File "/usr/lib/rpm/openruyi/pyproject_save_files.py", line 703, in generate_file_list
    raise ValueError(f"Globs did not match any module: {missed_text}")
ValueError: Globs did not match any module: zope_interface
"#;
        let issue = analyze_log(log);
        assert_eq!(
            issue,
            BuildIssue::InstallModuleMismatch {
                wrong_module: "zope_interface".into(),
                suggested_module: "zope_interface".into()
            }
        );
    }

    #[test]
    fn optional_integration_dedup_missing_modules() {
        let log = r#"/usr/lib/rpm/openruyi/import_all_modules.py
Check import: foo.integrations.bar
ModuleNotFoundError: No module named 'torch'
ModuleNotFoundError: No module named 'torch'
ModuleNotFoundError: No module named 'torch'
"#;
        let issue = analyze_log(log);
        match &issue {
            BuildIssue::ImportCheckExclusions {
                missing_modules, ..
            } => {
                assert_eq!(
                    missing_modules
                        .iter()
                        .filter(|m| m.as_str() == "torch")
                        .count(),
                    1,
                    "missing_modules must be deduplicated"
                );
            }
            _ => panic!("expected ImportCheckExclusions, got {issue:?}"),
        }
    }

    #[test]
    fn import_check_failures_are_grouped_by_parent_module() {
        let log = r#"/usr/lib/rpm/openruyi/import_all_modules.py
Failed to import: redis.asyncio.multidb.client, redis.asyncio.multidb.command_executor, redis.multidb.client
ModuleNotFoundError: No module named 'pybreaker'
"#;
        let issue = analyze_log(log);
        assert!(
            matches!(
                &issue,
                BuildIssue::ImportCheckExclusions { exclusions, .. }
                if exclusions == &vec![
                    "redis.asyncio.multidb.client".to_string(),
                    "redis.asyncio.multidb.command_executor".to_string(),
                    "redis.multidb.client".to_string()
                ]
            ),
            "failed imports must be grouped by parent module, got {issue:?}"
        );
    }

    #[test]
    fn import_check_failed_imports_directly_excluded() {
        // 现在所有 Failed to import 中的模块都直接排除，包括核心模块
        let log = r#"/usr/lib/rpm/openruyi/import_all_modules.py
Failed to import: foo.core, foo.optional
ModuleNotFoundError: No module named 'bar'
"#;
        let issue = analyze_log(log);
        assert!(
            matches!(issue, BuildIssue::ImportCheckExclusions { .. }),
            "failed imports should be directly excluded: {issue:?}"
        );
        if let BuildIssue::ImportCheckExclusions { exclusions, .. } = issue {
            assert_eq!(
                exclusions,
                vec!["foo.core".to_string(), "foo.optional".to_string()]
            );
        }
    }

    #[test]
    fn import_check_tests_module_excluded() {
        let log = r#"/usr/lib/rpm/openruyi/import_all_modules.py
Failed to import: zope.interface.tests.test_ro
ModuleNotFoundError: No module named 'zope.testing'
"#;
        let issue = analyze_log(log);
        assert!(matches!(
            issue,
            BuildIssue::ImportCheckExclusions { exclusions, .. }
            if exclusions == vec!["zope.interface.tests.test_ro".to_string()]
        ));
    }

    #[test]
    fn fonttools_optional_imports_excluded() {
        // fonttools 包中的可选导入失败应该被直接排除
        let log = r#"/usr/lib/rpm/openruyi/import_all_modules.py
Failed to import: fontTools.misc.symfont, fontTools.pens.freetypePen, fontTools.pens.quartzPen, fontTools.pens.reportLabPen, fontTools.ttLib.removeOverlaps, fontTools.varLib.interpolatablePlot, fontTools.varLib.plot
ModuleNotFoundError: No module named 'sympy'
"#;
        let issue = analyze_log(log);
        assert!(matches!(
            issue,
            BuildIssue::ImportCheckExclusions { exclusions, .. }
            if exclusions == vec![
                "fontTools.misc.symfont".to_string(),
                "fontTools.pens.freetypePen".to_string(),
                "fontTools.pens.quartzPen".to_string(),
                "fontTools.pens.reportLabPen".to_string(),
                "fontTools.ttLib.removeOverlaps".to_string(),
                "fontTools.varLib.interpolatablePlot".to_string(),
                "fontTools.varLib.plot".to_string(),
            ]
        ));
    }

    #[test]
    fn import_check_multidb_whitelist_excluded() {
        let log = r#"/usr/lib/rpm/openruyi/import_all_modules.py
Failed to import: redis.asyncio.multidb.client, redis.multidb.client
ModuleNotFoundError: No module named 'pybreaker'
"#;
        let issue = analyze_log(log);
        assert!(matches!(
            issue,
            BuildIssue::ImportCheckExclusions { exclusions, .. }
            if exclusions == vec![
                "redis.asyncio.multidb.client".to_string(),
                "redis.multidb.client".to_string()
            ]
        ));
    }

    #[test]
    fn unresolvable_with_prefix() {
        let log = "unresolvable: nothing provides python3dist(foo)";
        let issue = analyze_log(log);
        assert_eq!(
            issue,
            BuildIssue::DependencyUnresolvable {
                deps: vec!["python3dist(foo)".into()]
            }
        );
        assert!(matches!(
            decide_action(&issue),
            BuildAction::NeedHuman { .. }
        ));
    }

    #[test]
    fn unresolvable_dep_strips_punctuation_or_matches_exact_dist() {
        let log = "nothing provides python3dist(foo),\nnothing provides python3dist(bar)>=1\nnothing provides python3dist(baz).\n";
        let issue = analyze_log(log);
        assert_eq!(
            issue,
            BuildIssue::DependencyUnresolvable {
                deps: vec![
                    "python3dist(foo)".into(),
                    "python3dist(bar)".into(),
                    "python3dist(baz)".into()
                ]
            }
        );
    }

    #[test]
    fn unresolvable_supports_python313dist() {
        let issue = analyze_log("nothing provides python313dist(foo)");
        assert_eq!(
            issue,
            BuildIssue::DependencyUnresolvable {
                deps: vec!["python313dist(foo)".into()]
            }
        );
    }

    #[test]
    fn unresolvable_supports_python3_13dist() {
        let issue = analyze_log("unresolvable: nothing provides python3.13dist(unicodedata2)");
        assert_eq!(
            issue,
            BuildIssue::DependencyUnresolvable {
                deps: vec!["python3.13dist(unicodedata2)".into()]
            }
        );
    }

    #[test]
    fn unresolvable_supports_pkgconfig() {
        let issue = analyze_log("nothing provides pkgconfig(libxml-2.0)");
        assert_eq!(
            issue,
            BuildIssue::DependencyUnresolvable {
                deps: vec!["pkgconfig(libxml-2.0)".into()]
            }
        );
    }

    #[test]
    fn normal_module_not_found_is_need_human() {
        let log = "ModuleNotFoundError: No module named 'requests'";
        let issue = analyze_log(log);
        assert_eq!(
            issue,
            BuildIssue::MissingPythonModule {
                module: "requests".into(),
                import_context: None,
            }
        );
        assert!(matches!(
            decide_action(&issue),
            BuildAction::NeedHuman { .. }
        ));
    }

    #[test]
    fn pytest_module_not_found_is_test_failure() {
        let log = "pytest test_demo.py::test_import FAILED\nModuleNotFoundError: No module named 'foo'\nFAILED test_demo.py::test_import\n";
        let issue = analyze_log(log);
        assert!(matches!(issue, BuildIssue::TestFailure { .. }));
    }

    #[test]
    fn c_extension_module_not_found_if_has_fatal_error_is_c_extension() {
        let log = "fatal error: Python.h: No such file or directory\nModuleNotFoundError: No module named 'foo'\n";
        let issue = analyze_log(log);
        assert!(matches!(issue, BuildIssue::CExtensionCompileError { .. }));
    }

    #[test]
    fn normal_module_not_found_is_missing_python_module() {
        let log = "ModuleNotFoundError: No module named 'requests'";
        let issue = analyze_log(log);
        assert!(matches!(
            issue,
            BuildIssue::MissingPythonModule { module, .. } if module == "requests"
        ));
    }

    #[test]
    fn c_extension_compile_error_is_need_human() {
        let log = "fatal error: Python.h: No such file or directory\ncompilation terminated.\n";
        let issue = analyze_log(log);
        assert!(
            matches!(issue, BuildIssue::CExtensionCompileError { .. }),
            "expected CExtensionCompileError, got {issue:?}"
        );
        assert!(matches!(
            decide_action(&issue),
            BuildAction::NeedHuman { .. }
        ));
    }

    #[test]
    fn pytest_failure_is_need_human() {
        let log = "pytest test_something.py::test_foo FAILED\n";
        let issue = analyze_log(log);
        assert!(
            matches!(issue, BuildIssue::TestFailure { .. }),
            "expected TestFailure, got {issue:?}"
        );
        assert!(matches!(
            decide_action(&issue),
            BuildAction::NeedHuman { .. }
        ));
    }

    #[test]
    fn patch_apply_error_is_need_human() {
        let log = "patching file foo.py\nHunk #1 FAILED at 42.\n";
        let issue = analyze_log(log);
        assert!(
            matches!(issue, BuildIssue::PatchApplyError { .. }),
            "expected PatchApplyError, got {issue:?}"
        );
        assert!(matches!(
            decide_action(&issue),
            BuildAction::NeedHuman { .. }
        ));
    }
}
