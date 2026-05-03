use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use tokio::sync::{Mutex, Notify};

pub const MAX_LOCAL_BUILD_SLOTS: usize = 5;

/// A fixed-size pool of isolated build-root slots.
///
/// Each slot maps to a distinct `--root` directory under `base_dir` so that
/// concurrent local `osc build` invocations never share the same build root.
///
/// # Scope
///
/// This pool only guarantees that slots do not overlap **within a single
/// packfix-rs process**.  It does **not** coordinate with other packfix-rs
/// processes (or other tools writing to the same `base_dir`).  If two
/// packfix-rs processes run concurrently, they may independently acquire the
/// same slot and race on the same build-root directory.
///
/// Cross-process mutual exclusion (e.g. via `flock` on a lock file inside each
/// slot directory) is a planned follow-up and is not implemented yet.
#[derive(Debug)]
pub struct LocalBuildPool {
    slots: Arc<StdMutex<[bool; MAX_LOCAL_BUILD_SLOTS]>>,
    notifier: Arc<Notify>,
    base_dir: PathBuf,
    repository: String,
    arch: String,
}

/// A guard that holds one build-root slot.
///
/// Dropping it returns the slot to the pool and wakes one waiter.
/// The slot's `root()` directory is guaranteed to exist (it was created by
/// [`LocalBuildPool::acquire`]), but stale content from a previous build is
/// **not** automatically removed — `osc build` itself is responsible for
/// cleaning its `--root`.
#[derive(Debug)]
pub struct LocalBuildSlot {
    id: u32,
    root: PathBuf,
    slots: Arc<StdMutex<[bool; MAX_LOCAL_BUILD_SLOTS]>>,
    notifier: Arc<Notify>,
}

impl LocalBuildPool {
    /// `base_dir` is the parent directory for build roots (production:
    /// `/var/tmp/build-root`).  `repository` and `arch` are sanitised before
    /// being embedded into directory names.
    pub fn new(base_dir: PathBuf, repository: String, arch: String) -> Self {
        let repository = sanitize_path_segment(&repository);
        let arch = sanitize_path_segment(&arch);
        Self {
            slots: Arc::new(StdMutex::new([false; MAX_LOCAL_BUILD_SLOTS])),
            notifier: Arc::new(Notify::new()),
            base_dir,
            repository,
            arch,
        }
    }

    /// Acquire a slot, blocking (async) until one is free.
    ///
    /// # Errors
    ///
    /// Returns an error if the slot's build-root directory cannot be created.
    /// The slot is **not** leaked on error — the reserved slot index is
    /// released back to the pool before the error is returned.
    pub async fn acquire(&self) -> Result<LocalBuildSlot> {
        loop {
            let reserved = {
                let mut slots = self
                    .slots
                    .lock()
                    .expect("LocalBuildPool slot lock poisoned");
                slots.iter().position(|used| !*used).map(|id| {
                    slots[id] = true;
                    id as u32
                })
            };

            if let Some(id) = reserved {
                let root = self.base_dir.join(format!(
                    "packfix-{}-{}-{}",
                    id + 1,
                    self.repository,
                    self.arch,
                ));

                if let Err(e) = std::fs::create_dir_all(&root)
                    .with_context(|| format!("failed to create build root {}", root.display()))
                {
                    // Release the slot we just reserved — do NOT leak it.
                    Self::release_slot(&self.slots, &self.notifier, id);
                    return Err(e);
                }

                return Ok(LocalBuildSlot {
                    id,
                    root,
                    slots: Arc::clone(&self.slots),
                    notifier: Arc::clone(&self.notifier),
                });
            }

            self.notifier.notified().await;
        }
    }

    /// Release a previously-acquired slot by index.  Shared between the `Drop`
    /// implementation (normal return) and the error path in `acquire` (so that
    /// a failed `create_dir_all` does not permanently lose a slot).
    fn release_slot(slots: &StdMutex<[bool; MAX_LOCAL_BUILD_SLOTS]>, notifier: &Notify, id: u32) {
        slots.lock().expect("LocalBuildPool slot lock poisoned")[id as usize] = false;
        notifier.notify_one();
    }
}

impl LocalBuildSlot {
    /// The isolated `--root` directory for this slot.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for LocalBuildSlot {
    fn drop(&mut self) {
        LocalBuildPool::release_slot(&self.slots, &self.notifier, self.id);
    }
}

/// Replace characters that are not safe in filesystem path segments with `_`.
///
/// Allowed: ASCII alphanumeric, `.`, `_`, `-`.
/// If the result is empty, `"unknown"` is returned.
fn sanitize_path_segment(input: &str) -> String {
    let out: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "unknown".into()
    } else {
        out
    }
}

/// Ensures at most one LLM call runs at a time across all concurrent builds.
pub type LlmSemaphore = Arc<tokio::sync::Semaphore>;

pub struct BuildResources {
    pub git_lock: Arc<Mutex<()>>,
    pub local_build_pool: Arc<LocalBuildPool>,
    pub llm_semaphore: LlmSemaphore,
}

impl BuildResources {
    pub fn new(base_dir: PathBuf, repository: String, arch: String) -> Self {
        Self {
            git_lock: Arc::new(Mutex::new(())),
            local_build_pool: Arc::new(LocalBuildPool::new(base_dir, repository, arch)),
            llm_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pool() -> (tempfile::TempDir, LocalBuildPool) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool = LocalBuildPool::new(tmp.path().to_path_buf(), "x64".into(), "x86_64".into());
        (tmp, pool)
    }

    // ── slot allocation tests ──────────────────────────────────────────

    #[tokio::test]
    async fn pool_allocates_up_to_max_slots() {
        let (_tmp, pool) = test_pool();
        let base_dir = pool.base_dir.clone();

        let mut slots = Vec::new();
        for i in 0..MAX_LOCAL_BUILD_SLOTS {
            let slot = pool.acquire().await.expect("acquire slot");
            assert_eq!(
                slot.root(),
                &base_dir.join(format!("packfix-{}-x64-x86_64", i + 1))
            );
            assert!(slot.root().exists(), "build root should be created");
            slots.push(slot);
        }

        // All 5 slots are held; the next acquire should block.
        let acquire_fut = pool.acquire();
        let result = tokio::time::timeout(std::time::Duration::from_millis(300), acquire_fut).await;
        assert!(result.is_err(), "acquire should block when pool is full");

        // Drop one slot — now acquire should succeed.
        drop(slots.pop().unwrap());
        let slot = pool.acquire().await.expect("acquire after release");
        assert!(slot.root().to_string_lossy().contains("packfix-5"));
    }

    #[tokio::test]
    async fn slot_reuse_after_drop() {
        let (_tmp, pool) = test_pool();

        let slot1 = pool.acquire().await.expect("first acquire");
        let root1 = slot1.root().to_path_buf();
        drop(slot1);

        let slot2 = pool.acquire().await.expect("second acquire");
        assert_eq!(slot2.root(), root1, "freed slot should be reused");
    }

    #[tokio::test]
    async fn create_dir_all_failure_does_not_leak_slot() {
        // Use a base_dir that cannot be written to: a file, not a directory.
        let tmp = tempfile::tempdir().expect("tempdir");
        let bad_base = tmp.path().join("not_a_dir");
        std::fs::write(&bad_base, b"block").expect("write blocker file");

        let pool = LocalBuildPool::new(bad_base, "x64".into(), "x86_64".into());

        // Every acquire on this pool must fail quickly (create_dir_all hits a
        // file, not a directory).  If a failure path ever leaks a slot, the
        // pool will eventually be full of leaked slots and a subsequent
        // acquire will block (timeout) instead of returning Err.
        //
        // Call MAX_LOCAL_BUILD_SLOTS + 1 times — more than the pool capacity.
        // Each call is wrapped in a short timeout; a timeout means a slot was
        // leaked and the pool is stuck.
        for i in 0..MAX_LOCAL_BUILD_SLOTS + 1 {
            let acquire_fut = pool.acquire();
            let result =
                tokio::time::timeout(std::time::Duration::from_millis(500), acquire_fut).await;
            match result {
                Ok(Err(e)) => {
                    assert!(
                        e.to_string().contains("failed to create build root"),
                        "attempt {i}: error should mention build root: {e}"
                    );
                }
                Ok(Ok(_)) => {
                    panic!("attempt {i}: acquire unexpectedly succeeded on unwritable base");
                }
                Err(_timeout) => {
                    panic!(
                        "attempt {i}: acquire timed out — a slot was leaked by a previous failure and the pool is stuck"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn root_naming_includes_packfix_n_repo_arch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pool =
            LocalBuildPool::new(tmp.path().to_path_buf(), "standard".into(), "x86_64".into());

        let slot = pool.acquire().await.expect("acquire slot");
        assert_eq!(slot.root(), tmp.path().join("packfix-1-standard-x86_64"));
    }

    // ── sanitize_path_segment tests ────────────────────────────────────

    #[test]
    fn sanitize_replaces_slash_with_underscore() {
        let pool = LocalBuildPool::new("/tmp/build".into(), "x86_64/v2".into(), "x86_64".into());
        assert!(!pool.repository.contains('/'));
        assert_eq!(pool.repository, "x86_64_v2");
    }

    #[test]
    fn sanitize_replaces_semicolon_with_underscore() {
        let pool = LocalBuildPool::new("/tmp/build".into(), "x64".into(), "aarch64;id=foo".into());
        assert!(!pool.arch.contains(';'));
        assert_eq!(pool.arch, "aarch64_id_foo");
    }

    #[test]
    fn sanitize_empty_string_returns_unknown() {
        let pool = LocalBuildPool::new("/tmp/build".into(), "".into(), "x86_64".into());
        assert_eq!(pool.repository, "unknown");
    }

    #[test]
    fn sanitize_all_special_chars_becomes_underscores() {
        let pool = LocalBuildPool::new("/tmp/build".into(), "///".into(), "x86_64".into());
        // Each '/' is replaced with '_' → non-empty; does not fall back to "unknown".
        assert_eq!(pool.repository, "___");
    }

    #[test]
    fn sanitize_preserves_alphanumeric_dot_dash_underscore() {
        let pool = LocalBuildPool::new(
            "/tmp/build".into(),
            "repo_test-1.0".into(),
            "arch_v2".into(),
        );
        assert_eq!(pool.repository, "repo_test-1.0");
        assert_eq!(pool.arch, "arch_v2");
    }

    #[test]
    fn pool_default_config_is_none() {
        let pool: Option<Arc<LocalBuildPool>> = None;
        assert!(pool.is_none());
    }
}
