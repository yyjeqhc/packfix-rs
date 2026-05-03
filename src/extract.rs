//! Source-archive extraction after `osc checkout` / `osc up -S`.
//!
//! Finds the most likely main source archive in `source_dir` (e.g. the OBS
//! checkout directory), safely extracts it into
//! `.packfix/source-extract/<archive-stem>/` under `state_root` (e.g. the
//! packfix package workspace), and records the result in
//! `state_root/.packfix/state.json` and the operations log.
//!
//! Extraction failure is never fatal — it only logs a warning.

use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::utils::ops_log;

/// Status recorded for a single extraction attempt.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractStatus {
    Extracted,
    Skipped,
    Failed(String),
}

/// Serializable state stored in `.packfix/state.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackfixState {
    pub source_archive: Option<PathBuf>,
    pub source_extract_dir: Option<PathBuf>,
    pub extract_status: ExtractStatus,
    pub updated_at: String,
}

impl PackfixState {
    fn new() -> Self {
        Self {
            source_archive: None,
            source_extract_dir: None,
            extract_status: ExtractStatus::Skipped,
            updated_at: timestamp_now(),
        }
    }
}

/// Search `source_dir` for source archives, pick the best candidate, extract
/// it under `state_root`, and persist state to `state_root/.packfix/state.json`.
///
/// `source_dir` is where `*.tar.gz` etc. are looked for (e.g. the OBS checkout
/// directory).  `state_root` is where `.packfix/` state and extracted content
/// reside (e.g. the packfix package workspace).
///
/// Never panics; logs a warning on failure.
pub fn extract_source_if_present(source_dir: &Path, state_root: &Path) {
    if let Err(e) = try_extract(source_dir, state_root) {
        warn!(
            source_dir = %source_dir.display(),
            state_root = %state_root.display(),
            error = %e,
            "source extraction failed (non-fatal)"
        );
        ops_log::log_operation(
            state_root,
            "source-extract",
            &[format!("STATUS: failed"), format!("ERROR: {e}")],
        );
    }
}

fn try_extract(source_dir: &Path, state_root: &Path) -> Result<()> {
    let archives = find_source_archives(source_dir);
    if archives.is_empty() {
        info!(
            source_dir = %source_dir.display(),
            "no source archives found; skipping extraction"
        );
        ops_log::log_operation(
            state_root,
            "source-extract",
            &["STATUS: skipped (no source archive found)".into()],
        );
        let state = PackfixState::new();
        write_state(state_root, &state)?;
        return Ok(());
    }

    let best = pick_best_archive(&archives);
    let stem = archive_stem(&best);
    let extract_dir = state_root
        .join(".packfix")
        .join("source-extract")
        .join(&stem);

    info!(
        source_dir = %source_dir.display(),
        state_root = %state_root.display(),
        archive = %best.display(),
        extract_dir = %extract_dir.display(),
        "extracting source archive"
    );

    if extract_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&extract_dir) {
            warn!(
                extract_dir = %extract_dir.display(),
                error = %e,
                "failed to remove existing extract dir"
            );
        }
    }
    fs::create_dir_all(&extract_dir)
        .with_context(|| format!("failed to create extract dir {}", extract_dir.display()))?;

    extract_archive(&best, &extract_dir)?;

    let state = PackfixState {
        source_archive: Some(best.clone()),
        source_extract_dir: Some(extract_dir.clone()),
        extract_status: ExtractStatus::Extracted,
        updated_at: timestamp_now(),
    };
    write_state(state_root, &state)?;

    ops_log::log_operation(
        state_root,
        "source-extract",
        &[
            format!("SOURCE_DIR: {}", source_dir.display()),
            format!("ARCHIVE: {}", best.display()),
            format!("EXTRACT_DIR: {}", extract_dir.display()),
            "STATUS: extracted".into(),
        ],
    );

    info!(
        source_dir = %source_dir.display(),
        extract_dir = %extract_dir.display(),
        "source archive extracted"
    );
    Ok(())
}

// ── archive discovery ────────────────────────────────────────────────

/// File extensions we accept as source archives (in preference order).
const ARCHIVE_EXTENSIONS: &[&str] = &[".tar.gz", ".tar.xz", ".tar.bz2", ".tgz", ".zip"];

/// Extensions that should never be selected.
const IGNORED_EXTENSIONS: &[&str] = &[
    ".src.rpm", ".patch", ".diff", ".asc", ".sig", ".spec", ".service",
];

fn find_source_archives(workdir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(workdir) else {
        return Vec::new();
    };

    let mut archives: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if IGNORED_EXTENSIONS.iter().any(|ext| name.ends_with(ext)) {
                return false;
            }
            ARCHIVE_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
        })
        .collect();

    archives.sort();
    archives
}

/// Pick the "best" source archive:
/// 1. Prefer `.tar.gz` / `.tar.xz` / `.tar.bz2` / `.tgz` over `.zip`
/// 2. Break ties by file size (largest wins).
fn pick_best_archive(candidates: &[PathBuf]) -> PathBuf {
    debug_assert!(!candidates.is_empty());

    // Score: tar.* formats get 2, zip gets 1; then file size.
    let scored: Vec<(i32, u64, &PathBuf)> = candidates
        .iter()
        .map(|p| {
            let ext_score = if is_tar_archive(p) { 2 } else { 1 };
            let size = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            (ext_score, size, p)
        })
        .collect();

    scored
        .iter()
        .max_by_key(|(ext, size, _)| (*ext, *size))
        .map(|(_, _, p)| (*p).clone())
        .unwrap_or_else(|| candidates[0].clone())
}

fn is_tar_archive(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            name.ends_with(".tar.gz")
                || name.ends_with(".tar.xz")
                || name.ends_with(".tar.bz2")
                || name.ends_with(".tgz")
        })
}

fn archive_stem(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    // Strip known extensions in order: .tar.gz → .tgz → .tar.xz → .tar.bz2 → .zip
    if let Some(s) = name.strip_suffix(".tar.gz") {
        s.to_string()
    } else if let Some(s) = name.strip_suffix(".tar.xz") {
        s.to_string()
    } else if let Some(s) = name.strip_suffix(".tar.bz2") {
        s.to_string()
    } else if let Some(s) = name.strip_suffix(".tgz") {
        s.to_string()
    } else if let Some(s) = name.strip_suffix(".zip") {
        s.to_string()
    } else {
        name.to_string()
    }
}

// ── extraction ───────────────────────────────────────────────────────

fn extract_archive(archive_path: &Path, dest: &Path) -> Result<()> {
    let name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_tar_gz(archive_path, dest)
    } else if name.ends_with(".tar.bz2") {
        extract_tar_bz2(archive_path, dest)
    } else if name.ends_with(".tar.xz") {
        extract_tar_xz(archive_path, dest)
    } else if name.ends_with(".zip") {
        extract_zip(archive_path, dest)
    } else {
        bail!("unknown archive format: {}", archive_path.display());
    }
}

fn extract_tar_gz(archive_path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    extract_tar(decoder, dest)
}

fn extract_tar_bz2(archive_path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive_path)?;
    let decoder = bzip2::read::BzDecoder::new(file);
    extract_tar(decoder, dest)
}

fn extract_tar_xz(archive_path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive_path)?;
    let decoder = xz2::read::XzDecoder::new(file);
    extract_tar(decoder, dest)
}

fn extract_tar<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?;
        let path_str = entry_path.to_string_lossy();

        if !is_safe_entry_path(&path_str) {
            warn!(entry = %path_str, "skipping unsafe tar entry");
            continue;
        }

        let target = dest.join(entry_path.as_ref());
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&target)?;
    }
    Ok(())
}

fn extract_zip(archive_path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to open zip archive {}", archive_path.display()))?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let entry_name = entry.name().to_string();

        if !is_safe_entry_path(&entry_name) {
            warn!(entry = %entry_name, "skipping unsafe zip entry");
            continue;
        }

        let target = dest.join(&entry_name);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&target)?;
            std::io::copy(&mut entry, &mut out)?;
        }
    }
    Ok(())
}

// ── safety ───────────────────────────────────────────────────────────

/// Reject entries that could escape the destination directory.
fn is_safe_entry_path(path: &str) -> bool {
    if path.starts_with('/') {
        return false;
    }
    for component in path.split('/') {
        if component == ".." || component.starts_with("..\\") {
            return false;
        }
    }
    // Also reject Windows-style absolute paths
    if path.len() >= 3 {
        let bytes = path.as_bytes();
        if bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
            return false;
        }
    }
    true
}

// ── state persistence ────────────────────────────────────────────────

fn state_path(state_root: &Path) -> PathBuf {
    state_root.join(".packfix").join("state.json")
}

fn write_state(state_root: &Path, state: &PackfixState) -> Result<()> {
    let path = state_path(state_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)?;
    fs::write(&path, json)?;
    Ok(())
}

#[allow(dead_code)]
pub fn read_state(state_root: &Path) -> Result<Option<PackfixState>> {
    let path = state_path(state_root);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

fn timestamp_now() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("{}.{:03}Z", d.as_secs(), d.subsec_millis()),
        Err(_) => "unknown".to_string(),
    }
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_tar_gz(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let file = File::create(&path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(6);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "README.md", &b"# test\n"[..])
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn finds_tar_gz_archive() {
        let dir = tempdir().unwrap();
        create_tar_gz(dir.path(), "foo-1.0.tar.gz");
        let archives = find_source_archives(dir.path());
        assert_eq!(archives.len(), 1);
        assert!(archives[0].ends_with("foo-1.0.tar.gz"));
    }

    #[test]
    fn ignores_patch_and_sig_files() {
        let dir = tempdir().unwrap();
        File::create(dir.path().join("x.patch")).unwrap();
        File::create(dir.path().join("x.asc")).unwrap();
        File::create(dir.path().join("x.sig")).unwrap();
        File::create(dir.path().join("x.src.rpm")).unwrap();
        assert!(find_source_archives(dir.path()).is_empty());
    }

    #[test]
    fn no_archives_returns_skipped_no_error() {
        let dir = tempdir().unwrap();
        let result = try_extract(dir.path(), dir.path());
        assert!(result.is_ok());
        let state = read_state(dir.path()).unwrap().unwrap();
        assert!(matches!(state.extract_status, ExtractStatus::Skipped));
    }

    #[test]
    fn state_written_to_state_root_not_source_dir() {
        let source = tempdir().unwrap();
        let state_root = tempdir().unwrap();
        // Place a tar.gz in source_dir
        create_tar_gz(source.path(), "foo-1.0.tar.gz");
        let result = try_extract(source.path(), state_root.path());
        assert!(result.is_ok());
        // state.json must be under state_root, not source_dir
        assert!(
            state_root
                .path()
                .join(".packfix")
                .join("state.json")
                .exists()
        );
        assert!(!source.path().join(".packfix").exists());
        // source-extract must be under state_root
        let extract = state_root
            .path()
            .join(".packfix")
            .join("source-extract")
            .join("foo-1.0");
        assert!(extract.exists());
    }

    #[test]
    fn archive_stem_strips_extensions() {
        assert_eq!(archive_stem(Path::new("foo-1.0.tar.gz")), "foo-1.0");
        assert_eq!(archive_stem(Path::new("bar-2.0.tar.xz")), "bar-2.0");
        assert_eq!(archive_stem(Path::new("baz-3.0.tar.bz2")), "baz-3.0");
        assert_eq!(archive_stem(Path::new("qux.zip")), "qux");
        assert_eq!(archive_stem(Path::new("x.tgz")), "x");
    }

    #[test]
    fn picks_tar_gz_over_zip_regardless_of_size() {
        let dir = tempdir().unwrap();
        // Create a small tar.gz
        let tar_path = create_tar_gz(dir.path(), "pkg-1.0.tar.gz");
        // Create a zip file (just an empty file with .zip extension is
        // enough to test the selection heuristic).
        let zip_path = dir.path().join("small.zip");
        File::create(&zip_path).unwrap();

        // Make the zip larger than the tar.gz
        let tar_size = std::fs::metadata(&tar_path).unwrap().len();
        std::fs::write(&zip_path, vec![0u8; (tar_size * 2) as usize]).unwrap();

        let best = pick_best_archive(&[zip_path.clone(), tar_path.clone()]);
        // tar.gz should win over zip due to format preference, even though zip is larger
        assert_eq!(best, tar_path);
    }

    #[test]
    fn is_safe_entry_path_rejects_traversal() {
        assert!(!is_safe_entry_path("../evil"));
        assert!(!is_safe_entry_path("foo/../../etc/passwd"));
        assert!(!is_safe_entry_path("/etc/passwd"));
        assert!(is_safe_entry_path("foo/bar/baz.txt"));
        assert!(is_safe_entry_path("pkg-1.0/src/lib.rs"));
    }

    #[test]
    fn is_safe_entry_path_rejects_windows_absolute() {
        assert!(!is_safe_entry_path("C:\\Windows\\evil.exe"));
    }

    fn ensure_flate2_available() {
        // Sanity-check that the flate2 crate is compiled in.
        let _ = flate2::Compression::default();
    }

    #[test]
    fn compression_default_is_runtime_checkable() {
        ensure_flate2_available();
    }
}
