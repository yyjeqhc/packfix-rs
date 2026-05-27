use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Result, bail};

use crate::utils::command::{CommandSpec, run_command, run_command_blocking};

#[derive(Debug, Clone)]
pub struct TakopackResult {
    pub output_dir: PathBuf,
    pub spec_path: PathBuf,
    pub log_path: PathBuf,
}

pub fn python_package_name(name: &str) -> String {
    if name.starts_with("python-") {
        name.to_string()
    } else {
        format!("python-{name}")
    }
}

pub fn python_dist_name(name: &str) -> String {
    name.strip_prefix("python-").unwrap_or(name).to_string()
}

#[allow(dead_code)]
pub fn generate_python_spec(
    package_name: &str,
    version: Option<&str>,
    output_dir: &Path,
    takopack_bin: &Path,
) -> Result<TakopackResult> {
    let mut args = vec![
        "py".to_string(),
        "package".to_string(),
        "-o".to_string(),
        output_dir.display().to_string(),
        package_name.to_string(),
    ];
    if let Some(version) = version {
        args.push(version.to_string());
    }

    let log_path = output_dir.join("logs").join("takopack.log");
    let result = run_command_blocking(CommandSpec {
        program: takopack_bin.to_path_buf(),
        args,
        cwd: Some(output_dir.to_path_buf()),
        timeout: Duration::from_secs(600),
        log_path: log_path.clone(),
    })?;
    if result.returncode != 0 {
        bail!("takopack failed: {}", result.stderr);
    }

    let spec_path = find_generated_spec(output_dir, package_name);
    let Some(spec_path) = spec_path else {
        bail!(
            "takopack succeeded but no .spec was found under {}",
            output_dir.display()
        );
    };

    Ok(TakopackResult {
        output_dir: output_dir.to_path_buf(),
        spec_path,
        log_path,
    })
}

pub async fn generate_python_spec_async(
    package_name: &str,
    version: Option<&str>,
    output_dir: &Path,
    takopack_bin: &Path,
) -> Result<TakopackResult> {
    let mut args = vec![
        "py".to_string(),
        "package".to_string(),
        "-o".to_string(),
        output_dir.display().to_string(),
        package_name.to_string(),
    ];
    if let Some(version) = version {
        args.push(version.to_string());
    }

    let log_path = output_dir.join("logs").join("takopack.log");
    let result = run_command(CommandSpec {
        program: takopack_bin.to_path_buf(),
        args,
        cwd: Some(output_dir.to_path_buf()),
        timeout: Duration::from_secs(600),
        log_path: log_path.clone(),
    })
    .await?;
    if result.returncode != 0 {
        bail!("takopack failed: {}", result.stderr);
    }

    let spec_path = find_generated_spec(output_dir, package_name);
    let Some(spec_path) = spec_path else {
        bail!(
            "takopack succeeded but no .spec was found under {}",
            output_dir.display()
        );
    };

    Ok(TakopackResult {
        output_dir: output_dir.to_path_buf(),
        spec_path,
        log_path,
    })
}

fn find_generated_spec(output_dir: &Path, package_name: &str) -> Option<PathBuf> {
    let expected_dir = output_dir.join(format!("python-{package_name}"));
    find_spec_in(&expected_dir).or_else(|| find_spec_in(output_dir))
}

fn find_spec_in(dir: &Path) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "spec") {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = find_spec_in(&path)
        {
            return Some(found);
        }
    }
    None
}

use std::{fs::File, io::Read};

use flate2::read::GzDecoder;
use tar::Archive;

pub fn infer_install_module(workdir: &Path, wrong_module: &str) -> Result<Option<String>> {
    let wrong_module_lower = wrong_module.trim().replace('-', "_").to_lowercase();

    // 优先检查：从已解压的文件系统中读取 *.egg-info/top_level.txt
    let filesystem_top_levels = extract_top_levels_from_filesystem(workdir)?;
    if let Some(best) = select_top_level_candidate(&filesystem_top_levels, wrong_module) {
        return Ok(Some(best));
    }

    // 备选方案：从 tar 包中解压提取
    let archives = source_archives(workdir)?;

    for archive_path in archives {
        let entries = archive_entries(&archive_path)?;

        // 优先：如果归档内有 top_level.txt 指定的模块
        let archive_top_levels = top_level_modules(&entries, &archive_path)?;
        if let Some(best) = select_top_level_candidate(&archive_top_levels, wrong_module) {
            return Ok(Some(best));
        }

        // 新增：从归档中提取 Python 模块名（通过 __init__.py 路径）
        let python_modules = extract_python_modules_from_archive(&entries);

        // 优先：如果提取的模块与 wrong_module 匹配，返回它
        for module in python_modules.iter() {
            if top_level_matches_wrong_module(module, wrong_module) {
                return Ok(Some(module.clone()));
            }
        }

        // 备选：如果没有任何模块与 wrong_module 匹配，但归档中有明确的 Python 模块，
        // 直接返回第一个有效的 Python 模块（处理 wrong_module 完全错误的情况）
        if !python_modules.is_empty() {
            if let Some(non_dist_name) = python_modules
                .iter()
                .find(|module| module.to_lowercase() != wrong_module_lower)
            {
                return Ok(Some(non_dist_name.clone()));
            }
            return Ok(Some(python_modules[0].clone()));
        }

        // 最后备选：把归档内第一层目录名作为候选（例如 demo-1.0/src/pkg -> pkg）
        let archive_candidates = archive_first_level_candidates(&entries);
        for cand in archive_candidates.iter() {
            if top_level_matches_wrong_module(cand, wrong_module) {
                return Ok(Some(cand.clone()));
            }
        }

        for candidate in install_module_candidates(wrong_module) {
            if listing_contains_module(&entries, &candidate) {
                return Ok(Some(candidate));
            }
        }
    }

    // 备选方案：基于解包目录的一层目录启发式（不依赖 wheel/zip）
    if let Some(candidates) = first_level_candidates(workdir)? {
        for cand in candidates.iter() {
            if top_level_matches_wrong_module(cand, wrong_module) {
                return Ok(Some(cand.clone()));
            }
        }

        // 如果只有一个合理候选，作为保守回退返回它
        if let Some(non_dist_name) = candidates
            .iter()
            .find(|candidate| candidate.to_lowercase() != wrong_module_lower)
        {
            return Ok(Some(non_dist_name.clone()));
        }

        if candidates.len() == 1 {
            return Ok(Some(candidates[0].clone()));
        }
    }

    Ok(None)
}

fn source_archives(workdir: &Path) -> Result<Vec<PathBuf>> {
    let mut archives = Vec::new();
    for entry in std::fs::read_dir(workdir)? {
        let path = entry?.path();
        if path.is_file() && is_tar_gz(&path) {
            archives.push(path);
        }
    }
    archives.sort();
    Ok(archives)
}

fn extract_top_levels_from_filesystem(workdir: &Path) -> Result<Vec<String>> {
    // 递归查找 *.egg-info 目录
    fn find_top_level_in_dir(path: &Path, modules: &mut Vec<String>) -> Result<()> {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries {
                let entry = entry?;
                let entry_path = entry.path();
                let file_name = entry_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();

                // 检查是否是 .egg-info 目录
                if file_name.ends_with(".egg-info") && entry_path.is_dir() {
                    let top_level_path = entry_path.join("top_level.txt");
                    if top_level_path.exists()
                        && let Ok(content) = std::fs::read_to_string(&top_level_path)
                    {
                        for module in top_level_module_lines(&content) {
                            push_unique_module(modules, module);
                        }
                    }
                }

                // 递归进入子目录
                if entry_path.is_dir() {
                    find_top_level_in_dir(&entry_path, modules)?;
                }
            }
        }
        Ok(())
    }

    let mut modules = Vec::new();
    find_top_level_in_dir(workdir, &mut modules)?;
    Ok(modules)
}

fn is_tar_gz(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".tar.gz") || name.ends_with(".tgz"))
}

fn archive_entries(path: &Path) -> Result<Vec<String>> {
    let file = File::open(path)?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let mut entries = Vec::new();

    for item in archive.entries()? {
        let entry = item?;
        entries.push(entry.path()?.to_string_lossy().replace('\\', "/"));
    }

    Ok(entries)
}

fn top_level_modules(entries: &[String], archive_path: &Path) -> Result<Vec<String>> {
    let Some(top_level_path) = entries
        .iter()
        .find(|line| line.ends_with("/top_level.txt"))
        .map(String::as_str)
    else {
        return Ok(Vec::new());
    };

    let file = File::open(archive_path)?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    for item in archive.entries()? {
        let mut entry = item?;
        let path = entry.path()?.to_string_lossy().replace('\\', "/");
        if path == top_level_path {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            return Ok(top_level_module_lines(&text));
        }
    }
    Ok(Vec::new())
}

fn top_level_module_lines(text: &str) -> Vec<String> {
    let mut modules = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        push_unique_module(&mut modules, line.to_string());
    }
    modules
}

fn push_unique_module(modules: &mut Vec<String>, module: String) {
    if !modules.iter().any(|existing| existing == &module) {
        modules.push(module);
    }
}

fn select_top_level_candidate(top_levels: &[String], wrong_module: &str) -> Option<String> {
    top_levels
        .iter()
        .find(|top_level| top_level_matches_wrong_module(top_level, wrong_module))
        .cloned()
        .or_else(|| {
            top_levels
                .iter()
                .find(|top_level| {
                    !same_module_name(top_level, wrong_module)
                        && !is_auxiliary_module_name(top_level)
                })
                .cloned()
        })
        .or_else(|| {
            top_levels
                .iter()
                .find(|top_level| !same_module_name(top_level, wrong_module))
                .cloned()
        })
        .or_else(|| top_levels.first().cloned())
}

fn same_module_name(left: &str, right: &str) -> bool {
    left.trim().replace('-', "_").to_lowercase() == right.trim().replace('-', "_").to_lowercase()
}

fn is_auxiliary_module_name(module: &str) -> bool {
    matches!(
        module.trim().to_lowercase().as_str(),
        "example" | "examples" | "test" | "tests"
    )
}

fn top_level_matches_wrong_module(top_level: &str, wrong_module: &str) -> bool {
    let normalized_wrong = wrong_module.trim().replace('-', "_");

    // 精确匹配（原有逻辑）
    if normalized_wrong == top_level
        || normalized_wrong
            .split('_')
            .next()
            .is_some_and(|root| root == top_level)
    {
        return true;
    }

    // 大小写不敏感的匹配
    let wrong_lower = normalized_wrong.to_lowercase();
    let top_level_lower = top_level.to_lowercase();

    wrong_lower == top_level_lower
        || wrong_lower
            .split('_')
            .next()
            .is_some_and(|root| root == top_level_lower)
}

fn install_module_candidates(wrong_module: &str) -> Vec<String> {
    let normalized = wrong_module.trim().replace('-', "_");
    let dotted = normalized.replace('_', ".");
    let dotted_capitalized = capitalize_first(&dotted);

    // 生成多个候选项，包括大小写变体
    vec![
        normalized.clone(),
        dotted.clone(),
        capitalize_first(&normalized),
        dotted_capitalized,
        to_camel_case(&normalized),
    ]
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn to_camel_case(s: &str) -> String {
    s.split('_')
        .map(capitalize_first)
        .collect::<Vec<_>>()
        .join("")
}

fn listing_contains_module(entries: &[String], module: &str) -> bool {
    let module_path = module.replace('.', "/");
    let package_suffix = format!("/{module_path}/__init__.py");
    let module_suffix = format!("/{module_path}.py");

    // 大小写不敏感的模块路径用于匹配
    let module_path_lower = module_path.to_lowercase();
    let package_suffix_lower = format!("/{module_path_lower}/__init__.py");
    let module_suffix_lower = format!("/{module_path_lower}.py");
    let exact_package_lower = format!("{module_path_lower}/__init__.py");
    let exact_module_lower = format!("{module_path_lower}.py");

    entries.iter().any(|line| {
        // 大小写敏感的精确匹配（优先）
        if line.ends_with(&package_suffix)
            || line.ends_with(&module_suffix)
            || line == &format!("{module_path}/__init__.py")
            || line == &format!("{module_path}.py")
        {
            return true;
        }

        // 大小写不敏感的匹配（备选）
        let line_lower = line.to_lowercase();
        line_lower.ends_with(&package_suffix_lower)
            || line_lower.ends_with(&module_suffix_lower)
            || line_lower == exact_package_lower
            || line_lower == exact_module_lower
    })
}

// 基于解包目录的启发式：获取工作目录下第一层的候选模块名
// 过滤：以`.`开头的、tests/docs、常见构建/分发目录与 egg/dist-info
fn first_level_candidates(workdir: &Path) -> Result<Option<Vec<String>>> {
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(workdir) {
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            let name_lower = name.to_lowercase();
            if name.starts_with('.') {
                continue;
            }
            if name_lower.contains("test")
                || name_lower.contains("tests")
                || name_lower.contains("doc")
                || name_lower.contains("docs")
            {
                continue;
            }
            if ["build", "dist", "examples", "example", "logs"].contains(&name_lower.as_str()) {
                continue;
            }
            if name.ends_with(".egg-info") || name.ends_with(".dist-info") {
                continue;
            }

            if path.is_dir() {
                candidates.push(name);
            } else if path.is_file()
                && let Some(ext) = path.extension().and_then(|e| e.to_str())
                && ext == "py"
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                candidates.push(stem.to_string());
            }
        }
    }

    candidates.sort();
    candidates.dedup();
    if candidates.is_empty() {
        Ok(None)
    } else {
        Ok(Some(candidates))
    }
}

// 从归档 entries 中提取第一层候选目录名
fn archive_first_level_candidates(entries: &[String]) -> Vec<String> {
    let mut candidates = Vec::new();
    for entry in entries {
        // 标准化分隔符并忽略空项
        let p = entry.trim_start_matches('/');
        if p.is_empty() {
            continue;
        }
        // 获取第一段（顶层目录或文件名）
        if let Some(idx) = p.find('/') {
            let first = &p[..idx];
            let first_lower = first.to_lowercase();
            if first.starts_with('.') {
                continue;
            }
            if first_lower.contains("test")
                || first_lower.contains("tests")
                || first_lower.contains("doc")
                || first_lower.contains("docs")
            {
                continue;
            }
            if ["build", "dist", "examples", "example", "logs"].contains(&first_lower.as_str()) {
                continue;
            }
            if first.ends_with(".egg-info") || first.ends_with(".dist-info") {
                continue;
            }
            candidates.push(first.to_string());
        } else {
            // 没有 '/' 的单文件条目，使用文件名（去后缀）作为候选
            let name = p.split('/').next().unwrap_or("");
            if let Some(dot) = name.rfind('.') {
                let stem = &name[..dot];
                let stem_lower = stem.to_lowercase();
                if stem.starts_with('.') {
                    continue;
                }
                if stem_lower.contains("test")
                    || stem_lower.contains("tests")
                    || stem_lower.contains("doc")
                    || stem_lower.contains("docs")
                {
                    continue;
                }
                candidates.push(stem.to_string());
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

// 从归档路径中提取 Python 模块名（源码根目录下的顶层模块）
// 例如：
//   - scikit_build-0.19.0/skbuild/__init__.py -> skbuild
//   - demo-1.0/src/demo_pkg/__init__.py -> demo_pkg (跳过 src 等常见中间目录)
// 注意：只提取顶层模块，不提取子模块（如 skbuild/_compat/__init__.py 不应提取 _compat）
fn extract_python_modules_from_archive(entries: &[String]) -> Vec<String> {
    use std::collections::BTreeSet;

    // 常见的中间目录名，应该跳过
    let intermediate_dirs = ["src", "lib", "source", "sources"];

    let mut modules = BTreeSet::new();

    for entry in entries {
        let p = entry.trim_start_matches('/');
        if p.is_empty() {
            continue;
        }

        // 查找 __init__.py 文件
        if p.ends_with("/__init__.py") {
            // 分割路径
            let parts: Vec<&str> = p.split('/').collect();
            if parts.len() >= 3 {
                // parts[0] = 源码根目录 (例如 scikit_build-0.19.0 或 demo-1.0)
                // 寻找第一个不是中间目录的目录作为模块名
                let mut module_name: Option<&str> = None;

                // 从 parts[1] 开始查找（跳过源码根目录）
                for &candidate in parts.iter().take(parts.len() - 1).skip(1) {
                    let candidate_lower = candidate.to_lowercase();

                    // 跳过中间目录
                    if intermediate_dirs.contains(&candidate_lower.as_str()) {
                        continue;
                    }

                    // 跳过隐藏目录
                    if candidate.starts_with('.') {
                        continue;
                    }

                    // 找到第一个有效的模块名
                    module_name = Some(candidate);
                    break;
                }

                if let Some(module_name) = module_name {
                    let module_lower = module_name.to_lowercase();

                    // 过滤掉不需要的目录
                    if module_lower.contains("test") || module_lower.contains("tests") {
                        continue;
                    }
                    if ["build", "dist", "examples", "example", "logs"]
                        .contains(&module_lower.as_str())
                    {
                        continue;
                    }
                    if module_name.ends_with(".egg-info") || module_name.ends_with(".dist-info") {
                        continue;
                    }

                    modules.insert(module_name.to_string());
                }
            }
        }
    }

    modules.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::os::unix::fs::PermissionsExt;
    use tar::{Builder, Header};
    use tempfile::tempdir;

    #[test]
    fn generate_python_spec_passes_version_argument() {
        let dir = tempdir().expect("tempdir");
        let takopack = dir.path().join("takopack");
        let takopack_tmp = dir.path().join("takopack.tmp");
        let output = dir.path().join("out");
        std::fs::create_dir_all(&output).expect("create output");
        std::fs::write(
            &takopack_tmp,
            "#!/bin/sh\nprintf '%s\n' \"$@\" > \"$PWD/args.txt\"\nmkdir -p python-foo\nprintf 'Name: python-foo\n' > python-foo/python-foo.spec\n",
        )
        .expect("write fake takopack");
        std::fs::rename(&takopack_tmp, &takopack).expect("rename fake takopack");
        std::fs::set_permissions(&takopack, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let result =
            generate_python_spec("foo", Some("1.2.3"), &output, &takopack).expect("generate spec");

        assert_eq!(
            result.spec_path,
            output.join("python-foo").join("python-foo.spec")
        );
        let args = std::fs::read_to_string(output.join("args.txt")).expect("read args");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "py",
                "package",
                "-o",
                output.to_str().expect("utf8 output"),
                "foo",
                "1.2.3"
            ]
        );
    }

    #[test]
    fn takopack_receives_pypi_name_without_python_prefix() {
        let dir = tempdir().expect("tempdir");
        let takopack = dir.path().join("takopack");
        let takopack_tmp = dir.path().join("takopack.tmp");
        let output = dir.path().join("out");
        std::fs::create_dir_all(&output).expect("create output");
        std::fs::write(
            &takopack_tmp,
            "#!/bin/sh\nprintf '%s\n' \"$@\" > \"$PWD/args.txt\"\nmkdir -p python-fonttools\nprintf 'Name: python-fonttools\n' > python-fonttools/python-fonttools.spec\n",
        )
        .expect("write fake takopack");
        std::fs::rename(&takopack_tmp, &takopack).expect("rename fake takopack");
        std::fs::set_permissions(&takopack, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        generate_python_spec("fonttools", None, &output, &takopack).expect("generate spec");

        let args = std::fs::read_to_string(output.join("args.txt")).expect("read args");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "py",
                "package",
                "-o",
                output.to_str().expect("utf8 output"),
                "fonttools",
            ]
        );
    }

    #[test]
    fn takopack_output_does_not_pick_existing_unrelated_spec() {
        let dir = tempdir().expect("tempdir");
        let old_dir = dir.path().join("python-old");
        let new_dir = dir.path().join("python-new");
        std::fs::create_dir_all(&old_dir).expect("create old dir");
        std::fs::create_dir_all(&new_dir).expect("create new dir");
        std::fs::write(old_dir.join("python-old.spec"), "Name: python-old\n").expect("write old");
        std::fs::write(new_dir.join("python-new.spec"), "Name: python-new\n").expect("write new");

        let found = find_generated_spec(dir.path(), "new").expect("generated spec");
        assert_eq!(found, new_dir.join("python-new.spec"));
    }

    #[test]
    fn prefers_top_level_module_from_tarball() {
        let dir = tempdir().expect("tempdir");
        create_tarball(
            dir.path(),
            "demo-1.0.tar.gz",
            &[
                ("demo-1.0/src/zope/interface/__init__.py", ""),
                (
                    "demo-1.0/src/zope.interface.egg-info/top_level.txt",
                    "zope\n",
                ),
            ],
        );

        let inferred = infer_install_module(dir.path(), "zope_interface").expect("infer module");
        assert_eq!(inferred.as_deref(), Some("zope"));
    }

    #[test]
    fn falls_back_to_listing_when_no_top_level_exists() {
        let dir = tempdir().expect("tempdir");
        create_tarball(
            dir.path(),
            "demo-1.0.tar.gz",
            &[("demo-1.0/src/demo_pkg/__init__.py", "")],
        );

        let inferred = infer_install_module(dir.path(), "demo_pkg").expect("infer module");
        assert_eq!(inferred.as_deref(), Some("demo_pkg"));
    }

    #[test]
    fn handles_case_mismatch_in_top_level_txt() {
        let dir = tempdir().expect("tempdir");
        create_tarball(
            dir.path(),
            "fonttools-4.62.1.tar.gz",
            &[
                ("fonttools-4.62.1/src/fontTools/__init__.py", ""),
                (
                    "fonttools-4.62.1/src/fontTools.egg-info/top_level.txt",
                    "fontTools\n",
                ),
            ],
        );

        let inferred = infer_install_module(dir.path(), "fonttools").expect("infer module");
        assert_eq!(inferred.as_deref(), Some("fontTools"));
    }

    #[test]
    fn handles_case_mismatch_in_file_listing() {
        let dir = tempdir().expect("tempdir");
        create_tarball(
            dir.path(),
            "demo-1.0.tar.gz",
            &[("demo-1.0/src/DemoLib/__init__.py", "")],
        );

        let inferred = infer_install_module(dir.path(), "demolib").expect("infer module");
        // 返回实际的模块名 DemoLib（而不是小写的 demolib）
        assert_eq!(inferred.as_deref(), Some("DemoLib"));
    }

    #[test]
    fn prefers_filesystem_top_level_over_tarball() {
        let dir = tempdir().expect("tempdir");

        // 在文件系统中创建 .egg-info 目录
        let egg_info_dir = dir.path().join("fonttools-4.62.1.egg-info");
        std::fs::create_dir_all(&egg_info_dir).expect("create egg-info");
        std::fs::write(egg_info_dir.join("top_level.txt"), "fontTools\n")
            .expect("write top_level.txt");

        // 同时创建一个 tar 包（备选方案）
        create_tarball(
            dir.path(),
            "fonttools-4.62.1.tar.gz",
            &[("fonttools-4.62.1/src/fontTools/__init__.py", "")],
        );

        // 应该优先读取文件系统中的 top_level.txt
        let inferred = infer_install_module(dir.path(), "fonttools").expect("infer module");
        assert_eq!(inferred.as_deref(), Some("fontTools"));
    }

    #[test]
    fn prefers_primary_module_from_multiline_top_level() {
        let dir = tempdir().expect("tempdir");

        let egg_info_dir = dir.path().join("nvidia_ml_py.egg-info");
        std::fs::create_dir_all(&egg_info_dir).expect("create egg-info");
        std::fs::write(egg_info_dir.join("top_level.txt"), "example\npynvml\n")
            .expect("write top_level.txt");

        let inferred = infer_install_module(dir.path(), "nvidia_ml_py").expect("infer module");
        assert_eq!(inferred.as_deref(), Some("pynvml"));
    }

    #[test]
    fn infers_module_when_wrong_module_completely_different() {
        // 测试场景：wrong_module 完全错误（例如 logs），应该从 tar 包中提取真实模块名
        let dir = tempdir().expect("tempdir");
        create_tarball(
            dir.path(),
            "scikit_build-0.19.0.tar.gz",
            &[
                ("scikit_build-0.19.0/skbuild/__init__.py", ""),
                ("scikit_build-0.19.0/skbuild/cmaker.py", ""),
            ],
        );

        // wrong_module 是 "logs"（完全错误），应该推断出 "skbuild"
        let inferred = infer_install_module(dir.path(), "logs").expect("infer module");
        assert_eq!(inferred.as_deref(), Some("skbuild"));
    }

    fn create_tarball(root: &Path, archive_name: &str, files: &[(&str, &str)]) {
        let archive_path = root.join(archive_name);
        let file = File::create(&archive_path).expect("archive file");
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);

        for (path, content) in files {
            let mut header = Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, *path, content.as_bytes())
                .expect("append file");
        }

        let encoder = builder.into_inner().expect("tar encoder");
        encoder.finish().expect("gzip finish");
    }
}
