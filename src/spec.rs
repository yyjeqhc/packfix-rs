use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use regex::Regex;
use tracing::warn;

pub fn find_spec(workdir: &Path) -> Result<PathBuf> {
    let mut found = Vec::new();
    collect_specs(workdir, &mut found)?;
    found.sort();
    found
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no .spec found under {}", workdir.display()))
}

pub fn read_spec(path: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(path)?)
}

pub fn tag_value(spec: &str, tag: &str) -> Option<String> {
    let prefix = format!("{tag}:");
    spec.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
    })
}

pub fn declared_package_name(spec: &str) -> Option<String> {
    for line in spec.lines() {
        let (key, value) = line.split_once(':')?;
        if key.trim() != "Name" {
            continue;
        }
        let value = value.trim();
        if value.is_empty() || value.contains("%{") {
            return None;
        }
        return Some(value.to_string());
    }
    None
}

pub fn write_spec(path: &Path, text: &str) -> Result<()> {
    let normalized = if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    };
    std::fs::write(path, normalized)?;
    Ok(())
}

pub fn add_buildrequires(spec: &str, deps: &[String]) -> String {
    if deps.is_empty() {
        return spec.to_string();
    }
    let mut existing = BTreeSet::new();
    for line in spec.lines() {
        if let Some(dep) = line.strip_prefix("BuildRequires:") {
            let dep = dep.trim().to_string();
            existing.insert(dep_identity(&dep));
            existing.insert(dep);
        }
    }

    let mut additions = Vec::new();
    for dep in deps {
        let dep = dep.trim();
        if dep.is_empty() {
            continue;
        }
        let identity = dep_identity(dep);
        if existing.contains(dep) || existing.contains(&identity) {
            continue;
        }
        additions.push(format!("BuildRequires:  {dep}"));
        existing.insert(identity);
        existing.insert(dep.to_string());
    }
    if additions.is_empty() {
        return spec.to_string();
    }

    let mut lines: Vec<String> = spec.lines().map(ToString::to_string).collect();
    let insert_at = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with("BuildRequires:"))
        .map(|(i, _)| i + 1)
        .next_back()
        .or_else(|| lines.iter().position(|line| line.starts_with('%')))
        .unwrap_or(lines.len());
    lines.splice(insert_at..insert_at, additions);
    normalize_spec(&lines.join("\n"))
}

pub fn append_files_entries(spec: &str, files: &[String]) -> String {
    let mut additions = Vec::new();
    for file in files {
        let file = file.trim();
        if file.starts_with('/') {
            additions.push(path_to_macro(file));
        }
    }
    additions.sort();
    additions.dedup();
    if additions.is_empty() {
        return spec.to_string();
    }

    let mut lines: Vec<String> = spec.lines().map(ToString::to_string).collect();
    let Some(files_idx) = lines.iter().position(|line| line.starts_with("%files")) else {
        // no %files section: append one and the entries
        lines.push("%files".to_string());
        lines.extend(additions);
        return normalize_spec(&lines.join("\n"));
    };
    let mut end_idx = section_end(&lines, files_idx + 1);
    // collect existing (trimmed) entries between %files and next section header
    let existing: BTreeSet<String> = lines[files_idx + 1..end_idx]
        .iter()
        .map(|line| line.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let to_add: Vec<String> = additions
        .into_iter()
        .filter(|entry| !existing.contains(entry))
        .collect();
    if to_add.is_empty() {
        return spec.to_string();
    }
    // If there are no existing non-empty entries, insert immediately after %files.
    // Also remove any blank lines right after %files to avoid an empty gap.
    let insert_at = if existing.is_empty() {
        files_idx + 1
    } else {
        end_idx
    };
    if insert_at == files_idx + 1 {
        // remove consecutive empty lines after %files
        while insert_at < lines.len() && lines[insert_at].trim().is_empty() {
            lines.remove(insert_at);
            // end_idx shifts left when removing
            end_idx = end_idx.saturating_sub(1);
        }
    }
    // Insert new entries
    lines.splice(insert_at..insert_at, to_add);
    // Ensure there's exactly one blank line before the next section header
    let next_hdr = section_end(&lines, files_idx + 1);
    if next_hdr < lines.len() {
        if next_hdr == 0 || lines[next_hdr - 1].trim().is_empty() {
            // already has blank line before header
        } else {
            lines.insert(next_hdr, String::new());
        }
    }
    normalize_spec(&lines.join("\n"))
}

pub fn fix_buildoption_install(spec: &str, arg: &str) -> String {
    let re = Regex::new(r"(?m)^BuildOption\(install\):\s*(.*)$").expect("valid regex");
    let Some(caps) = re.captures(spec) else {
        return spec.to_string();
    };
    let full = caps.get(0).expect("full match").as_str();
    let mut args = split_args(caps.get(1).map_or("", |m| m.as_str()));

    if arg == "-L" {
        if !args.iter().any(|item| item == "-L") {
            args.push("-L".to_string());
        }
    } else {
        let mut replaced = false;
        for idx in 0..args.len() {
            if args[idx] == "-l" && idx + 1 < args.len() {
                args[idx + 1] = arg.to_string();
                replaced = true;
                break;
            }
        }
        if !replaced {
            args.push("-l".to_string());
            args.push(arg.to_string());
        }
    }

    spec.replacen(
        full,
        &format!("BuildOption(install):  {}", args.join(" ")),
        1,
    )
}

pub fn add_buildoption_checks(spec: &str, exclusions: &[String]) -> String {
    let normalized: Vec<String> = exclusions
        .iter()
        .map(|exclusion| exclusion.trim().to_string())
        .filter(|exclusion| !exclusion.is_empty())
        .collect();
    if normalized.is_empty() {
        return spec.to_string();
    }

    let mut lines: Vec<String> = spec.lines().map(ToString::to_string).collect();
    let existing = existing_buildoption_check_exclusions(&lines);
    let mut to_add = Vec::new();
    for exclusion in normalized {
        if existing
            .iter()
            .any(|item| exclusion_is_covered(item, &exclusion))
        {
            continue;
        }
        to_add.push(format!("BuildOption(check):  -e \"{exclusion}\""));
    }
    if to_add.is_empty() {
        return spec.to_string();
    }
    let insert_at = lines
        .iter()
        .rposition(|line| line.starts_with("BuildOption(check):"))
        .map(|idx| idx + 1)
        .or_else(|| {
            lines
                .iter()
                .position(|line| line.starts_with("BuildOption(install):"))
                .map(|idx| idx + 1)
        })
        .or_else(|| {
            lines
                .iter()
                .position(|line| line.starts_with("BuildSystem:"))
                .map(|idx| idx + 1)
        })
        .unwrap_or_else(|| {
            lines
                .iter()
                .position(|line| line.starts_with('%'))
                .unwrap_or(lines.len())
        });
    lines.splice(insert_at..insert_at, to_add);
    normalize_spec(&lines.join("\n"))
}

pub fn add_extras_subpackages(spec: &str, features: &[String]) -> String {
    if features.is_empty() {
        return spec.to_string();
    }

    let lines: Vec<&str> = spec.lines().collect();
    let Some(desc_idx) = lines.iter().position(|line| *line == "%description") else {
        return spec.to_string();
    };
    let section_end_idx = section_end_str(&lines, desc_idx + 1);
    let extras_end_idx = extras_block_end_str(&lines, section_end_idx);

    let mut new_lines: Vec<String> = Vec::new();
    new_lines.extend(lines[..=desc_idx].iter().map(|line| (*line).to_string()));
    new_lines.extend(
        lines[desc_idx + 1..section_end_idx]
            .iter()
            .map(|line| (*line).to_string()),
    );
    if section_end_idx > desc_idx + 1 && !new_lines.last().is_some_and(|line| line.is_empty()) {
        new_lines.push(String::new());
    }
    for feature in features {
        let feature = feature.trim();
        if !feature.is_empty() {
            new_lines.push(format!(
                "%pyproject_extras_subpkg -n python-%{{srcname}} {feature}"
            ));
        }
    }
    new_lines.push(String::new());
    new_lines.extend(
        lines[extras_end_idx..]
            .iter()
            .map(|line| (*line).to_string()),
    );
    normalize_spec(&new_lines.join("\n"))
}

pub fn fix_buildarch_remove_noarch(spec: &str) -> String {
    let mut lines: Vec<String> = spec
        .lines()
        .filter(|line| {
            !Regex::new(r"^BuildArch:\s*noarch\s*$")
                .expect("valid regex")
                .is_match(line)
        })
        .map(ToString::to_string)
        .collect();
    lines.retain(|line| {
        !line.starts_with("Provides:       python3-%{srcname}")
            && !line.starts_with("%python_provide python3-%{srcname}")
    });

    let provides = vec![
        "Provides:       python3-%{srcname} = %{version}-%{release}".to_string(),
        "Provides:       python3-%{srcname}%{?_isa} = %{version}-%{release}".to_string(),
        "%python_provide python3-%{srcname}".to_string(),
    ];
    let insert_at = lines
        .iter()
        .position(|line| {
            line.starts_with("BuildRequires:")
                || line.starts_with("BuildOption")
                || line.starts_with("BuildSystem:")
        })
        .unwrap_or_else(|| {
            lines
                .iter()
                .position(|line| line.starts_with('%'))
                .unwrap_or(lines.len())
        });
    lines.splice(insert_at..insert_at, provides);
    normalize_spec(&lines.join("\n"))
}

pub fn update_summary(spec: &str, summary: &str) -> String {
    replace_or_insert_tag(spec, "Summary", summary)
}

pub fn update_version(spec: &str, version: &str) -> String {
    replace_or_insert_tag(spec, "Version", version)
}

pub fn ensure_remote_asset(spec: &str, remote_asset_line: &str) -> String {
    let replacement = remote_asset_line.trim();
    let mut lines: Vec<String> = spec.lines().map(ToString::to_string).collect();
    if let Some(idx) = lines
        .iter()
        .position(|line| line.trim_start().starts_with("#!RemoteAsset"))
    {
        lines[idx] = replacement.to_string();
        return normalize_spec(&lines.join("\n"));
    }

    let insert_at = lines
        .iter()
        .position(|line| line.starts_with("Source"))
        .unwrap_or_else(|| {
            lines
                .iter()
                .position(|line| line.starts_with('%'))
                .unwrap_or(lines.len())
        });
    lines.insert(insert_at, replacement.to_string());
    normalize_spec(&lines.join("\n"))
}

pub fn ensure_buildarch_noarch(spec: &str) -> String {
    replace_or_insert_tag(spec, "BuildArch", "noarch")
}

pub fn ensure_versioned_python_provides(spec: &str) -> String {
    let mut lines: Vec<String> = spec.lines().map(ToString::to_string).collect();
    let replacement = [
        "Provides:       python3-%{srcname} = %{version}-%{release}".to_string(),
        "%python_provide python3-%{srcname}".to_string(),
    ];
    let matched_indexes: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            if let Some(rest) = line.strip_prefix("Provides:") {
                let trimmed = rest.trim_start();
                if trimmed.starts_with("python3-") {
                    let space_count = rest.len() - trimmed.len();
                    if space_count != 7 {
                        warn!(
                            "Provides line at index {idx} has {space_count} spaces after colon (expected 7)"
                        );
                    }
                    return Some(idx);
                }
            }
            if line.starts_with("%python_provide python3-") {
                return Some(idx);
            }
            None
        })
        .collect();

    if let Some(first_idx) = matched_indexes.first().copied() {
        let mut new_lines = Vec::with_capacity(lines.len() + replacement.len());
        for (idx, line) in lines.into_iter().enumerate() {
            if idx == first_idx {
                new_lines.extend(replacement.iter().cloned());
            }
            if matched_indexes.contains(&idx) {
                continue;
            }
            new_lines.push(line);
        }
        return normalize_spec(&new_lines.join("\n"));
    }

    let insert_at = lines
        .iter()
        .position(|line| {
            line.starts_with("BuildRequires:")
                || line.starts_with("BuildOption")
                || line.starts_with("BuildSystem:")
        })
        .unwrap_or_else(|| {
            lines
                .iter()
                .position(|line| line.starts_with('%'))
                .unwrap_or(lines.len())
        });
    lines.splice(insert_at..insert_at, replacement);
    normalize_spec(&lines.join("\n"))
}

pub fn ensure_autochangelog_macro(spec: &str) -> String {
    spec.replace("%{?autochangelog}", "%autochangelog")
}

pub fn add_empty_check_section(spec: &str, comment: &str) -> String {
    if spec.lines().any(|line| line.trim() == "%check") {
        return spec.to_string();
    }

    let mut lines: Vec<String> = spec.lines().map(ToString::to_string).collect();
    let insert_at = lines
        .iter()
        .position(|line| line.starts_with("%files"))
        .unwrap_or_else(|| {
            lines
                .iter()
                .position(|line| line.starts_with("%changelog"))
                .unwrap_or(lines.len())
        });

    let mut block = vec!["%check".to_string()];
    if !comment.trim().is_empty() {
        block.push(format!("# {}", comment.trim()));
    }
    block.push(String::new());
    lines.splice(insert_at..insert_at, block);
    normalize_spec(&lines.join("\n"))
}

pub fn update_description(spec: &str, description: &str) -> String {
    let lines: Vec<&str> = spec.lines().collect();
    let Some(start) = lines.iter().position(|line| *line == "%description") else {
        return spec.to_string();
    };
    let end = section_end_str(&lines, start + 1);
    let mut new_lines: Vec<String> = lines[..=start].iter().map(|s| (*s).to_string()).collect();
    new_lines.push(description.trim().to_string());
    new_lines.push(String::new()); // blank line before next section
    new_lines.extend(lines[end..].iter().map(|s| (*s).to_string()));
    normalize_spec(&new_lines.join("\n"))
}

pub fn normalize_spec(spec: &str) -> String {
    let mut lines: Vec<String> = spec
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    format!("{}\n", lines.join("\n"))
}

fn collect_specs(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        bail!("workdir does not exist: {}", dir.display());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_specs(&path, found)?;
        } else if path.extension().is_some_and(|ext| ext == "spec") {
            found.push(path);
        }
    }
    Ok(())
}

fn dep_identity(dep: &str) -> String {
    let re = Regex::new(r"\s+(?:[<>=]=?|=)\s+").expect("valid regex");
    re.split(dep.trim()).next().unwrap_or(dep).to_string()
}

fn path_to_macro(path: &str) -> String {
    for (prefix, macro_name) in [
        ("/usr/bin/", "%{_bindir}/"),
        ("/usr/sbin/", "%{_sbindir}/"),
        ("/usr/libexec/", "%{_libexecdir}/"),
        ("/usr/share/man/", "%{_mandir}/"),
        ("/usr/share/", "%{_datadir}/"),
        ("/etc/", "%{_sysconfdir}/"),
        ("/usr/include/", "%{_includedir}/"),
        ("/usr/lib64/", "%{_libdir}/"),
        ("/usr/lib/", "%{_prefix}/lib/"),
    ] {
        if let Some(rest) = path.strip_prefix(prefix) {
            return format!("{macro_name}{rest}");
        }
    }
    path.to_string()
}

fn section_end(lines: &[String], start: usize) -> usize {
    lines[start..]
        .iter()
        .position(|line| is_section_header(line))
        .map_or(lines.len(), |offset| start + offset)
}

fn section_end_str(lines: &[&str], start: usize) -> usize {
    lines[start..]
        .iter()
        .position(|line| is_section_header(line))
        .map_or(lines.len(), |offset| start + offset)
}

fn extras_block_end_str(lines: &[&str], start: usize) -> usize {
    let mut idx = start;
    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }
    while idx < lines.len() && is_extras_subpkg_line(lines[idx]) {
        idx += 1;
    }
    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }
    idx
}

fn is_extras_subpkg_line(line: &str) -> bool {
    line.trim_start()
        .starts_with("%pyproject_extras_subpkg -n python-%{srcname} ")
}

fn is_section_header(line: &str) -> bool {
    Regex::new(r"^%[A-Za-z_]\w*(?:\s|$)")
        .expect("valid regex")
        .is_match(line)
        && !line.starts_with("%dir")
        && !line.starts_with("%doc")
        && !line.starts_with("%license")
}

fn split_args(text: &str) -> Vec<String> {
    let re = Regex::new(r#""[^"]*"|\S+"#).expect("valid regex");
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

fn existing_buildoption_check_exclusions(lines: &[String]) -> Vec<String> {
    let re = Regex::new(r#"(?m)^BuildOption\(check\):\s*(.*)$"#).expect("valid regex");
    let mut exclusions = Vec::new();
    for line in lines {
        if let Some(caps) = re.captures(line) {
            let args = split_args(caps.get(1).map_or("", |m| m.as_str()));
            for pair in args.windows(2) {
                if pair[0] == "-e" {
                    let exclusion = pair[1].trim_matches('"').to_string();
                    if !exclusions.contains(&exclusion) {
                        exclusions.push(exclusion);
                    }
                }
            }
        }
    }
    exclusions
}

fn exclusion_is_covered(existing: &str, candidate: &str) -> bool {
    if existing == candidate {
        return true;
    }
    existing
        .strip_suffix(".*")
        .is_some_and(|prefix| candidate.starts_with(prefix))
}

fn replace_or_insert_tag(spec: &str, tag: &str, value: &str) -> String {
    let prefix = format!("{tag}:");
    let formatted = format!("{prefix:<16}{}", value.trim());
    let mut lines: Vec<String> = spec.lines().map(ToString::to_string).collect();
    if let Some(idx) = lines.iter().position(|line| line.starts_with(&prefix)) {
        lines[idx] = formatted;
        return normalize_spec(&lines.join("\n"));
    }
    let insert_at = lines
        .iter()
        .position(|line| line.starts_with('%'))
        .unwrap_or(lines.len());
    lines.insert(insert_at, formatted);
    normalize_spec(&lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_buildoption_check_merge() {
        let spec = "Name: foo\nBuildOption(check):  -e \"foo.tests.*\"\n%description\nx\n";
        let once = add_buildoption_checks(spec, &[String::from("lmformatenforcer.integrations.*")]);
        let twice =
            add_buildoption_checks(&once, &[String::from("lmformatenforcer.integrations.*")]);
        assert!(twice.contains("BuildOption(check):  -e \"foo.tests.*\""));
        assert!(twice.contains("BuildOption(check):  -e \"lmformatenforcer.integrations.*\""));
        assert_eq!(twice.matches("lmformatenforcer.integrations.*").count(), 1);
    }

    #[test]
    fn add_buildrequires_idempotent() {
        let spec = "Name: foo\nBuildRequires:  python3dist(pytest)\n%description\nx\n";
        let out = add_buildrequires(spec, &[String::from("python3dist(pytest)")]);
        assert_eq!(out.matches("python3dist(pytest)").count(), 1);
    }

    #[test]
    fn append_files_entries_idempotent() {
        let spec = "Name: foo\n%files -f %{pyproject_files}\n%changelog\n";
        let once = append_files_entries(spec, &[String::from("/usr/bin/foo")]);
        let twice = append_files_entries(&once, &[String::from("/usr/bin/foo")]);
        assert_eq!(twice.matches("%{_bindir}/foo").count(), 1);
    }

    #[test]
    fn fix_buildoption_install_l_idempotent() {
        let spec = "BuildOption(install):  -l foo\n%description\nx\n";
        let once = fix_buildoption_install(spec, "-L");
        let twice = fix_buildoption_install(&once, "-L");
        assert_eq!(twice.matches("-L").count(), 1);
    }

    #[test]
    fn add_buildrequires_with_version_constraint_idempotent() {
        let spec = "Name: foo\nBuildRequires:  python3dist(pytest) >= 7\n%description\nx\n";
        let out = add_buildrequires(spec, &[String::from("python3dist(pytest)")]);
        assert_eq!(out.matches("python3dist(pytest)").count(), 1);
    }

    #[test]
    fn append_files_entries_ignores_relative_paths() {
        let spec = "Name: foo\n%files -f %{pyproject_files}\n%changelog\n";
        let out = append_files_entries(spec, &[String::from("relative/path")]);
        assert!(!out.contains("relative/path"));
    }

    #[test]
    fn append_files_entries_maps_common_paths() {
        let spec = "Name: foo\n%files -f %{pyproject_files}\n%changelog\n";
        let out = append_files_entries(
            spec,
            &[
                String::from("/usr/bin/foo"),
                String::from("/usr/sbin/fooctl"),
                String::from("/usr/libexec/food"),
                String::from("/usr/include/foo.h"),
                String::from("/etc/foo.conf"),
            ],
        );
        assert!(out.contains("%{_bindir}/foo"));
        assert!(out.contains("%{_sbindir}/fooctl"));
        assert!(out.contains("%{_libexecdir}/food"));
        assert!(out.contains("%{_includedir}/foo.h"));
        assert!(out.contains("%{_sysconfdir}/foo.conf"));
    }

    #[test]
    fn add_buildoption_check_merge_multiple_existing_exclusions() {
        let spec = "Name: foo\nBuildOption(check):  -e \"foo.tests.*\"\nBuildOption(check):  -e \"bar.bench.*\"\n%description\nx\n";
        let out = add_buildoption_checks(spec, &[String::from("lmformatenforcer.integrations.*")]);
        assert!(out.contains("BuildOption(check):  -e \"foo.tests.*\""));
        assert!(out.contains("BuildOption(check):  -e \"bar.bench.*\""));
        assert!(out.contains("BuildOption(check):  -e \"lmformatenforcer.integrations.*\""));
    }

    #[test]
    fn add_buildoption_check_idempotent() {
        let spec = "Name: foo\nBuildOption(check):  -e \"foo.tests.*\"\n%description\nx\n";
        let once = add_buildoption_checks(spec, &[String::from("foo.tests.*")]);
        let twice = add_buildoption_checks(&once, &[String::from("foo.tests.*")]);
        assert_eq!(twice.matches("foo.tests.*").count(), 1);
    }

    #[test]
    fn add_buildoption_check_adds_one_line_per_exclusion() {
        let spec = "Name: foo\nBuildOption(install):  -l foo\n%description\nx\n";
        let out = add_buildoption_checks(
            spec,
            &[
                String::from("redis.asyncio.multidb.*"),
                String::from("redis.multidb.*"),
            ],
        );
        assert!(out.contains("BuildOption(check):  -e \"redis.asyncio.multidb.*\""));
        assert!(out.contains("BuildOption(check):  -e \"redis.multidb.*\""));
        assert_eq!(out.matches("BuildOption(check):").count(), 2);
    }

    #[test]
    fn add_buildoption_check_skips_when_existing_pattern_is_broader() {
        let spec = "Name: foo\nBuildOption(check):  -e \"redis.multidb.*\"\n%description\nx\n";
        let out = add_buildoption_checks(spec, &[String::from("redis.multidb.client.*")]);
        assert_eq!(out.matches("BuildOption(check):").count(), 1);
        assert!(!out.contains("redis.multidb.client.*"));
    }

    #[test]
    fn fix_buildoption_install_no_line_no_panic() {
        let spec = "Name: foo\n%description\nx\n";
        let out = fix_buildoption_install(spec, "-L");
        assert_eq!(out, spec);
    }

    #[test]
    fn fix_buildoption_install_replace_l_arg_preserve_other_args() {
        let spec = "BuildOption(install):  -l wrong_module --long-flag -L\n%description\nx\n";
        let out = fix_buildoption_install(spec, "correct_module");
        assert!(out.contains("-l correct_module"));
        assert!(out.contains("--long-flag"));
        assert!(out.contains("-L"));
        assert!(!out.contains("wrong_module"));
    }

    #[test]
    fn fix_buildarch_remove_noarch_removes_buildarch() {
        let spec = "Name: foo\nBuildArch: noarch\n%description\nx\n";
        let out = fix_buildarch_remove_noarch(spec);
        assert!(!out.contains("BuildArch: noarch"));
    }

    #[test]
    fn fix_buildarch_remove_noarch_adds_arch_provides() {
        let spec = "Name: foo\n%description\nx\n";
        let out = fix_buildarch_remove_noarch(spec);
        assert!(out.contains("Provides:       python3-%{srcname} = %{version}-%{release}"));
        assert!(out.contains("Provides:       python3-%{srcname}%{?_isa} = %{version}-%{release}"));
        assert!(out.contains("%python_provide python3-%{srcname}"));
    }

    #[test]
    fn fix_buildarch_remove_noarch_idempotent() {
        let spec = "Name: foo\nBuildArch: noarch\n%description\nx\n";
        let once = fix_buildarch_remove_noarch(spec);
        let twice = fix_buildarch_remove_noarch(&once);
        assert_eq!(
            twice.matches("Provides:       python3-%{srcname}").count(),
            2
        );
        assert!(!twice.contains("BuildArch: noarch"));
    }

    #[test]
    fn ensure_versioned_python_provides_replaces_unversioned_pair() {
        let spec = "Name: foo\nProvides:       python3-%{srcname}\n%python_provide python3-%{srcname}\nBuildRequires:  python3dist(setuptools)\n";
        let out = ensure_versioned_python_provides(spec);
        assert!(out.contains("Provides:       python3-%{srcname} = %{version}-%{release}"));
        assert!(!out.contains("Provides:       python3-%{srcname}\n%python_provide"));
        assert_eq!(out.matches("%python_provide python3-%{srcname}").count(), 1);
    }

    #[test]
    fn ensure_versioned_python_provides_preserves_original_position() {
        let spec = "Name: foo\nBuildArch:      noarch\nProvides:       python3-%{srcname}\n%python_provide python3-%{srcname}\nBuildSystem:    pyproject\n";
        let out = ensure_versioned_python_provides(spec);
        assert!(out.contains(
            "BuildArch:      noarch\nProvides:       python3-%{srcname} = %{version}-%{release}\n%python_provide python3-%{srcname}\nBuildSystem:    pyproject"
        ));
    }

    #[test]
    fn ensure_versioned_python_provides_replaces_literal_package_name() {
        let spec = "Name: python-socks\nProvides:       python3-socks\nBuildRequires:  python3dist(setuptools)\n";
        let out = ensure_versioned_python_provides(spec);
        assert!(out.contains("Provides:       python3-%{srcname} = %{version}-%{release}"));
        assert!(out.contains("%python_provide python3-%{srcname}"));
        assert!(!out.contains("Provides:       python3-socks\n"));
    }

    #[test]
    fn ensure_versioned_python_provides_replaces_literal_pair() {
        let spec = "Name: python-socks\nProvides:       python3-socks\n%python_provide python3-socks\nBuildRequires:  python3dist(setuptools)\n";
        let out = ensure_versioned_python_provides(spec);
        assert!(out.contains("Provides:       python3-%{srcname} = %{version}-%{release}"));
        assert_eq!(out.matches("%python_provide python3-%{srcname}").count(), 1);
        assert!(!out.contains("python3-socks"));
    }

    #[test]
    fn ensure_versioned_python_provides_skips_already_versioned_literal() {
        let spec = "Name: python-socks\nProvides:       python3-socks = %{version}-%{release}\nBuildRequires:  python3dist(setuptools)\n";
        let out = ensure_versioned_python_provides(spec);
        assert!(out.contains("Provides:       python3-%{srcname} = %{version}-%{release}"));
        assert!(!out.contains("python3-socks"));
    }

    #[test]
    fn ensure_versioned_python_provides_preserves_literal_position() {
        // Simulates the real python-socks spec: provides at the bottom, after BuildRequires
        let spec = "\
Name:           python-socks
Version:        2.8.0
BuildArch:      noarch
BuildSystem:    pyproject
BuildRequires:  pyproject-rpm-macros
BuildRequires:  python3dist(setuptools)

Provides:       python3-socks
%python_provide python3-socks
";
        let out = ensure_versioned_python_provides(spec);
        // Provides should be replaced with macro form
        assert!(out.contains("Provides:       python3-%{srcname} = %{version}-%{release}"));
        assert_eq!(out.matches("%python_provide python3-%{srcname}").count(), 1);
        assert!(!out.contains("python3-socks"));
        // Position must be preserved: after BuildRequires, not before BuildSystem
        let provides_pos = out.find("Provides:").unwrap();
        let buildreq_pos = out.find("BuildRequires:  pyproject").unwrap();
        let buildsys_pos = out.find("BuildSystem:").unwrap();
        assert!(
            provides_pos > buildreq_pos,
            "Provides should be after BuildRequires, not before it"
        );
        assert!(
            provides_pos > buildsys_pos,
            "Provides should be after BuildSystem, not before it"
        );
    }

    #[test]
    fn ensure_buildarch_noarch_inserts_missing_tag() {
        let spec = "Name: foo\nBuildSystem:    pyproject\n";
        let out = ensure_buildarch_noarch(spec);
        assert!(out.contains("BuildArch:      noarch"));
    }

    #[test]
    fn ensure_remote_asset_replaces_or_inserts_line() {
        let spec = "Name: foo\n#!RemoteAsset\nSource0:        https://files.pythonhosted.org/packages/source/f/foo/foo-1.0.tar.gz\n";
        let out = ensure_remote_asset(spec, "#!RemoteAsset:  sha256:abc123");
        assert!(out.contains("#!RemoteAsset:  sha256:abc123"));

        let inserted = ensure_remote_asset(
            "Name: foo\nSource0:        https://files.pythonhosted.org/packages/source/f/foo/foo-1.0.tar.gz\n",
            "#!RemoteAsset:  sha256:def456",
        );
        assert!(inserted.contains("#!RemoteAsset:  sha256:def456"));
    }

    #[test]
    fn update_version_replaces_existing_value() {
        let spec = "Name: foo\nVersion:        1.0.0\n";
        let out = update_version(spec, "1.2.3");
        assert!(out.contains("Version:        1.2.3"));
    }

    #[test]
    fn ensure_autochangelog_macro_replaces_conditional_form() {
        let spec = "%changelog\n%{?autochangelog}\n";
        let out = ensure_autochangelog_macro(spec);
        assert!(out.contains("%changelog\n%autochangelog\n"));
        assert!(!out.contains("%{?autochangelog}"));
    }

    #[test]
    fn add_empty_check_section_inserts_before_files() {
        let spec =
            "%generate_buildrequires\n%pyproject_buildrequires\n\n%files -f %{pyproject_files}\n";
        let out = add_empty_check_section(
            spec,
            "No importable runtime modules for default import check.",
        );
        assert!(out.contains(
            "%generate_buildrequires\n%pyproject_buildrequires\n\n%check\n# No importable runtime modules for default import check.\n\n%files -f %{pyproject_files}\n"
        ));
    }

    #[test]
    fn add_empty_check_section_keeps_existing_check() {
        let spec = "%check\n# existing\n\n%files -f %{pyproject_files}\n";
        let out = add_empty_check_section(spec, "ignored");
        assert_eq!(out, spec);
    }

    #[test]
    fn add_extras_subpackages_preserves_description_body_and_surrounding_blank_lines() {
        let spec = "Name: python-fonttools\n%description\nFontTools body line 1\nFontTools body line 2\n\n%prep\n";
        let out = add_extras_subpackages(spec, &["lxml".into(), "unicode".into()]);
        assert!(out.contains("%description\nFontTools body line 1\nFontTools body line 2\n\n%pyproject_extras_subpkg -n python-%{srcname} lxml\n%pyproject_extras_subpkg -n python-%{srcname} unicode\n\n%prep"));
    }

    #[test]
    fn declared_package_name_ignores_macro_expansion_names() {
        assert_eq!(
            declared_package_name("Name: python-demo\n"),
            Some("python-demo".into())
        );
        assert_eq!(declared_package_name("Name: python-%{srcname}\n"), None);
    }

    #[test]
    fn update_description_replaces_content() {
        let before = "Name: foo\n%description\nOld description text\n\n%prep\n";
        let new_desc = "New concise description.";
        let after = update_description(before, new_desc);
        assert!(after.contains("New concise description."));
        assert!(!after.contains("Old description text"));
        // The blank line between description body and next section is preserved.
        assert!(after.contains("\n\n%prep"));
    }

    #[test]
    fn update_description_empty_clears_body_but_keeps_tag() {
        let before = "Name: foo\n%description\nOld description text\n%prep\n";
        let after = update_description(before, "");
        assert!(after.contains("%description"));
        assert!(!after.contains("Old description text"));
    }

    #[test]
    fn update_description_no_section_returns_unchanged() {
        let before = "Name: foo\n%prep\n";
        let after = update_description(before, "New desc");
        assert_eq!(after, before);
    }
}
