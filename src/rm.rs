//! Guarded scratch removal: plan first, quarantine on apply, restore or
//! separately-approved purge afterwards.
//!
//! Nothing here deletes user data on first pass. `rm --apply` moves guarded
//! targets into a same-filesystem quarantine by atomic no-replace rename;
//! only `purge --apply` removes bytes, and only for entries whose recorded
//! identity still matches. There is no force flag: protection failures abort
//! the whole batch before any mutation.
//!
//! Recovery model: every mutation is journaled per entry with crash-safe
//! journal writes, and apply/restore/purge resume interrupted plans instead
//! of rejecting them. Quarantine destinations are unique per plan entry, so
//! duplicate basenames can never collide.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::guard::{self, GuardedPath};

static PLAN_COUNTER: AtomicU64 = AtomicU64::new(0);
static JOURNAL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Default scratch root when `--root` is not given.
pub const DEFAULT_SCRATCH_ROOT: &str = "/data/scratch/tmp/opencode";

/// Quarantine directory name inside the allowed root (same device, so
/// retirement is always a rename, never copy-and-delete).
pub const QUARANTINE_DIR_NAME: &str = ".doty-quarantine";

fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DOTY_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".local/state/doty")
}

fn plans_dir() -> PathBuf {
    state_dir().join("rm-plans")
}

fn new_plan_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = PLAN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("rm-{}-{}-{}", nanos, std::process::id(), counter)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTarget {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub is_dir: bool,
    #[serde(default)]
    pub quarantined_as: Option<PathBuf>,
    #[serde(default)]
    pub restored: bool,
    #[serde(default)]
    pub purged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RmPlan {
    pub id: String,
    pub created_at: String,
    pub root: PathBuf,
    pub quarantine_dir: PathBuf,
    pub targets: Vec<PlannedTarget>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RmPreview {
    pub id: String,
    pub root: PathBuf,
    pub targets: Vec<PathBuf>,
}

fn plan_path(id: &str) -> PathBuf {
    plans_dir().join(format!("{id}.json"))
}

fn load_plan(id: &str) -> Result<RmPlan> {
    if id.contains('/') || id.contains('\0') || id.starts_with('.') {
        anyhow::bail!("invalid plan id: {id}");
    }
    let content = std::fs::read_to_string(plan_path(id))
        .with_context(|| format!("unknown removal plan: {id}"))?;
    serde_json::from_str(&content).with_context(|| format!("cannot parse plan {id}"))
}

/// Crash-safe journal write: temp file + fsync + atomic rename + dir fsync.
/// A crash can never leave a truncated journal behind.
fn journal_write(plan: &RmPlan) -> Result<()> {
    let dir = plans_dir();
    std::fs::create_dir_all(&dir).with_context(|| "cannot create plan directory")?;
    let counter = JOURNAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{}.tmp-{}-{}",
        plan.id,
        std::process::id(),
        counter
    ));
    let content = serde_json::to_string_pretty(plan)?;
    std::fs::write(&tmp, content)
        .with_context(|| format!("cannot write journal {}", tmp.display()))?;
    std::fs::File::open(&tmp)
        .with_context(|| format!("cannot reopen journal {}", tmp.display()))?
        .sync_all()
        .with_context(|| format!("cannot fsync journal {}", tmp.display()))?;
    std::fs::rename(&tmp, plan_path(&plan.id))
        .with_context(|| format!("cannot publish plan {}", plan.id))?;
    std::fs::File::open(&dir)
        .with_context(|| "cannot open plan directory")?
        .sync_all()
        .with_context(|| "cannot fsync plan directory")?;
    Ok(())
}

fn save_plan(plan: &RmPlan) -> Result<()> {
    let path = plan_path(&plan.id);
    if path.exists() {
        anyhow::bail!("plan id collision: {}", path.display());
    }
    journal_write(plan)
}

/// Atomic no-replace rename. Fails (EEXIST) if anything — file, dir, or
/// dangling symlink — already occupies the destination. No check/use race.
fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let from_c = CString::new(from.as_os_str().as_bytes())
        .with_context(|| format!("cannot encode {}", from.display()))?;
    let to_c = CString::new(to.as_os_str().as_bytes())
        .with_context(|| format!("cannot encode {}", to.display()))?;
    // SAFETY: both pointers are valid NUL-terminated C strings borrowed for
    // the call; AT_FDCWD resolves them as plain paths; RENAME_NOREPLACE is
    // a flag-only argument with no pointer semantics.
    let ret = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!(
            "refusing rename (destination occupied or not replaceable): {} -> {}: {err}",
            from.display(),
            to.display()
        );
    }
    Ok(())
}

/// Refuse nested protection inside a directory tree: repository markers,
/// operator holds, and mount boundaries. Never follows symlinks.
fn validate_tree(path: &Path, expected_dev: u64) -> Result<()> {
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let meta = std::fs::symlink_metadata(&current)
            .with_context(|| format!("cannot stat {}", current.display()))?;
        if meta.dev() != expected_dev {
            anyhow::bail!("refusing mount boundary inside {}", current.display());
        }
        if current != path {
            let name = current
                .file_name()
                .with_context(|| format!("cannot name {}", current.display()))?;
            if name == ".git" || name == crate::guard::PROTECT_MARKER {
                anyhow::bail!("refusing protected content inside {}", current.display());
            }
        }
        if meta.is_dir() {
            for entry in std::fs::read_dir(&current)
                .with_context(|| format!("cannot list {}", current.display()))?
            {
                stack.push(entry?.path());
            }
        }
    }
    Ok(())
}

/// Deterministic per-entry destination: unique even for duplicate basenames,
/// stable across resume retries.
fn quarantine_destination(plan: &RmPlan, index: usize, source: &Path) -> Result<PathBuf> {
    let file_name = source
        .file_name()
        .with_context(|| format!("cannot quarantine {}", source.display()))?;
    Ok(plan.quarantine_dir.join(format!(
        "{}-{index}-{}",
        plan.id,
        PathBuf::from(file_name).display()
    )))
}

/// Validate targets and record a plan. Writes nothing on any failure.
pub fn plan_removal(root: &Path, targets: &[PathBuf]) -> Result<RmPreview> {
    if targets.is_empty() {
        anyhow::bail!("no removal targets given");
    }
    let root_canonical = root
        .canonicalize()
        .with_context(|| format!("cannot resolve root {}", root.display()))?;
    let quarantine_dir = root_canonical.join(QUARANTINE_DIR_NAME);
    let mut planned = Vec::with_capacity(targets.len());
    for target in targets {
        let guarded: GuardedPath = guard::guard_path(target, root)?;
        if guarded.path == root_canonical {
            anyhow::bail!(
                "refusing to remove the allowed root itself: {}",
                guarded.path.display()
            );
        }
        if guarded.path == quarantine_dir || guarded.path.starts_with(&quarantine_dir) {
            anyhow::bail!(
                "refusing to remove the quarantine directory: {}",
                guarded.path.display()
            );
        }
        if guarded.is_dir {
            validate_tree(&guarded.path, guarded.dev)?;
        }
        planned.push(PlannedTarget {
            path: guarded.path,
            dev: guarded.dev,
            ino: guarded.ino,
            is_dir: guarded.is_dir,
            quarantined_as: None,
            restored: false,
            purged: false,
        });
    }
    // Duplicate and overlapping targets would quarantine/restore over each
    // other. Target counts here are CLI-scale, so the pairwise check is fine.
    // ponytail: O(n^2) path-prefix scan; index by components if plans grow large.
    for (i, a) in planned.iter().enumerate() {
        for b in planned.iter().skip(i + 1) {
            if a.path == b.path {
                anyhow::bail!("duplicate removal target: {}", a.path.display());
            }
            if a.path.starts_with(&b.path) || b.path.starts_with(&a.path) {
                anyhow::bail!(
                    "overlapping removal targets: {} and {}",
                    a.path.display(),
                    b.path.display()
                );
            }
        }
    }
    let plan = RmPlan {
        id: new_plan_id(),
        created_at: chrono_now_rfc3339(),
        root: root_canonical,
        quarantine_dir,
        targets: planned,
    };
    let preview = RmPreview {
        id: plan.id.clone(),
        root: plan.root.clone(),
        targets: plan
            .targets
            .iter()
            .map(|target| target.path.clone())
            .collect(),
    };
    save_plan(&plan)?;
    Ok(preview)
}

fn chrono_now_rfc3339() -> String {
    // std-only RFC3339-ish timestamp (no chrono dependency in this crate).
    use std::time::SystemTime;
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => format!("unix:{}", duration.as_secs()),
        Err(_) => "unix:unknown".to_string(),
    }
}

/// Describe a recorded plan without mutating anything. Works for unapplied
/// plans (review before apply) as well as in-progress ones.
pub fn describe_plan(id: &str) -> Result<RmPlan> {
    load_plan(id)
}

/// Quarantine pending entries by atomic no-replace rename. Entries already
/// quarantined are re-verified and skipped, so interrupted batches resume
/// instead of stranding the plan.
pub fn apply_plan(id: &str) -> Result<RmPlan> {
    let plan = load_plan(id)?;
    let mut journaled = plan.clone();
    // Whole-batch preflight for pending entries first: identity + policy.
    let mut pending: Vec<usize> = Vec::new();
    for (index, target) in plan.targets.iter().enumerate() {
        if let Some(destination) = target.quarantined_as.as_ref() {
            // Resume path: the recorded entry must still be the same object.
            let meta = std::fs::symlink_metadata(destination).with_context(|| {
                format!(
                    "quarantined entry missing for {} (plan {id} cannot resume)",
                    target.path.display()
                )
            })?;
            if meta.dev() != target.dev || meta.ino() != target.ino {
                anyhow::bail!(
                    "quarantined entry identity changed: {} (plan {id} cannot resume)",
                    destination.display()
                );
            }
            continue;
        }
        if target.restored || target.purged {
            anyhow::bail!(
                "plan {id} entry {} is neither quarantined nor pending; restore or purge it instead",
                target.path.display()
            );
        }
        let guarded = GuardedPath {
            path: target.path.clone(),
            dev: target.dev,
            ino: target.ino,
            is_dir: target.is_dir,
        };
        guard::revalidate(&guarded, &plan.root)?;
        pending.push(index);
    }
    if pending.is_empty() {
        return Ok(journaled);
    }
    std::fs::create_dir_all(&plan.quarantine_dir)
        .with_context(|| format!("cannot create quarantine {}", plan.quarantine_dir.display()))?;
    // The quarantine itself is never a future removal candidate.
    let marker = plan.quarantine_dir.join(crate::guard::PROTECT_MARKER);
    if std::fs::symlink_metadata(&marker).is_err() {
        std::fs::write(&marker, "doty quarantine; do not remove by hand\n")
            .with_context(|| format!("cannot write {}", marker.display()))?;
    }

    for index in pending {
        let target = &plan.targets[index];
        let destination = quarantine_destination(&plan, index, &target.path)?;
        rename_noreplace(&target.path, &destination).with_context(|| {
            format!(
                "cannot quarantine {} (journaled progress kept in plan {id})",
                target.path.display()
            )
        })?;
        journaled.targets[index].quarantined_as = Some(destination);
        // Journal each move immediately so an interrupted batch is resumable
        // by inspection, never silently half-done.
        journal_write(&journaled)?;
    }
    Ok(journaled)
}

/// Move quarantined entries back. Already-restored entries are skipped so
/// interrupted restores resume; refuses to overwrite existing paths.
pub fn restore(id: &str) -> Result<RmPlan> {
    let plan = load_plan(id)?;
    let mut journaled = plan.clone();
    // Two-phase: verify every pending restore before moving the first.
    let mut pending: Vec<usize> = Vec::new();
    for (index, target) in plan.targets.iter().enumerate() {
        let Some(destination) = target.quarantined_as.as_ref() else {
            if target.restored {
                continue;
            }
            anyhow::bail!(
                "plan {id} entry {} was never applied; nothing to restore",
                target.path.display()
            );
        };
        // The quarantined entry must still be the same object we moved.
        let meta = std::fs::symlink_metadata(destination)
            .with_context(|| format!("quarantined entry missing: {}", destination.display()))?;
        if meta.dev() != target.dev || meta.ino() != target.ino {
            anyhow::bail!(
                "quarantined entry identity changed: {}",
                destination.display()
            );
        }
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "quarantined entry became a symlink: {}",
                destination.display()
            );
        }
        if std::fs::symlink_metadata(&target.path).is_ok() {
            anyhow::bail!(
                "restore destination exists (refusing overwrite): {}",
                target.path.display()
            );
        }
        pending.push(index);
    }
    if pending.is_empty() {
        return Ok(journaled);
    }
    for index in pending {
        let destination = journaled.targets[index]
            .quarantined_as
            .clone()
            .expect("checked above");
        let id = journaled.id.clone();
        rename_noreplace(&destination, &journaled.targets[index].path).with_context(|| {
            format!(
                "cannot restore {} (journaled progress kept in plan {id})",
                destination.display()
            )
        })?;
        journaled.targets[index].quarantined_as = None;
        journaled.targets[index].restored = true;
        journal_write(&journaled)?;
    }
    Ok(journaled)
}

/// Describe what `purge --apply` would remove. Read-only; works at any stage.
pub fn purge_preview(id: &str) -> Result<RmPlan> {
    load_plan(id)
}

/// Permanently remove quarantined entries after revalidating identity and
/// nested protection. Already-purged entries are skipped so interrupted
/// purges resume. This is the only function in doty that deletes approved
/// scratch data.
pub fn purge_apply(id: &str) -> Result<RmPlan> {
    let plan = load_plan(id)?;
    let mut journaled = plan.clone();
    // Two-phase: verify every pending purge before deleting the first.
    let mut pending: Vec<usize> = Vec::new();
    for (index, target) in plan.targets.iter().enumerate() {
        if target.purged {
            continue;
        }
        let Some(destination) = target.quarantined_as.as_ref() else {
            anyhow::bail!(
                "plan {id} entry {} is not quarantined; purge needs an applied plan",
                target.path.display()
            );
        };
        let meta = std::fs::symlink_metadata(destination)
            .with_context(|| format!("quarantined entry missing: {}", destination.display()))?;
        if meta.dev() != target.dev || meta.ino() != target.ino {
            anyhow::bail!(
                "quarantined entry identity changed: {}",
                destination.display()
            );
        }
        if meta.file_type().is_symlink() {
            anyhow::bail!("refusing symlink at purge time: {}", destination.display());
        }
        if meta.is_dir() {
            validate_tree(destination, target.dev)?;
        }
        pending.push(index);
    }
    if pending.is_empty() {
        return Ok(journaled);
    }
    for index in pending {
        let destination = journaled.targets[index]
            .quarantined_as
            .clone()
            .expect("checked above");
        let expected = journaled.targets[index].clone();
        // Re-check identity at deletion time: the preflight above and this
        // check bracket the batch, but entries are removed one by one.
        let meta = std::fs::symlink_metadata(&destination)
            .with_context(|| format!("quarantined entry missing: {}", destination.display()))?;
        if meta.dev() != expected.dev || meta.ino() != expected.ino {
            anyhow::bail!(
                "quarantined entry identity changed: {}",
                destination.display()
            );
        }
        if meta.file_type().is_symlink() {
            anyhow::bail!("refusing symlink at purge time: {}", destination.display());
        }
        if meta.is_dir() {
            std::fs::remove_dir_all(&destination)
                .with_context(|| format!("cannot purge {}", destination.display()))?;
        } else {
            std::fs::remove_file(&destination)
                .with_context(|| format!("cannot purge {}", destination.display()))?;
        }
        journaled.targets[index].quarantined_as = None;
        journaled.targets[index].purged = true;
        journal_write(&journaled)?;
    }
    Ok(journaled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Plan storage resolves through the process-global DOTY_STATE_DIR, so
    // tests that redirect it must not run concurrently.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp root")
    }

    fn with_state_dir(root: &Path) -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = ENV_LOCK.lock().expect("env lock");
        let state = tempfile::tempdir().expect("state dir");
        // SAFETY: tests hold ENV_LOCK, so no concurrent env access occurs.
        unsafe { std::env::set_var("DOTY_STATE_DIR", state.path()) };
        let _ = root;
        (state, guard)
    }

    #[test]
    fn plan_rejects_unapproved_paths() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        // Outside the root.
        let outside = temp_root();
        let victim = outside.path().join("data.txt");
        std::fs::write(&victim, "x").unwrap();
        assert!(plan_removal(root.path(), &[victim]).is_err());
    }

    #[test]
    fn apply_restore_roundtrip() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "keep me").unwrap();

        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        assert!(file.exists());
        let applied = apply_plan(&preview.id).unwrap();
        assert!(!file.exists());
        let quarantined = applied.targets[0].quarantined_as.clone().unwrap();
        assert!(quarantined.exists());

        let restored = restore(&preview.id).unwrap();
        assert!(restored.targets[0].restored);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me");
    }

    #[test]
    fn restore_refuses_overwrite() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "original").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        apply_plan(&preview.id).unwrap();
        // Something reappears at the original path.
        std::fs::write(&file, "new occupant").unwrap();
        let err = restore(&preview.id).unwrap_err();
        assert!(err.to_string().contains("refusing overwrite"), "{err}");
    }

    #[test]
    fn purge_needs_quarantine_first() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), &[file]).unwrap();
        let err = purge_apply(&preview.id).unwrap_err();
        assert!(err.to_string().contains("not quarantined"), "{err}");
    }

    #[test]
    fn purge_removes_quarantined_bytes() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), &[file]).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let quarantined = applied.targets[0].quarantined_as.clone().unwrap();
        let purged = purge_apply(&preview.id).unwrap();
        assert!(purged.targets[0].purged);
        assert!(!quarantined.exists());
    }

    #[test]
    fn stale_plan_after_replace_is_rejected() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "v1").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        let err = apply_plan(&preview.id).unwrap_err();
        assert!(err.to_string().contains("identity changed"), "{err}");
    }

    #[test]
    fn duplicate_basenames_quarantine_distinctly() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let a = root.path().join("a");
        let b = root.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let fa = a.join("file.txt");
        let fb = b.join("file.txt");
        std::fs::write(&fa, "aaa").unwrap();
        std::fs::write(&fb, "bbb").unwrap();
        let preview = plan_removal(root.path(), &[fa.clone(), fb.clone()]).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let da = applied.targets[0].quarantined_as.clone().unwrap();
        let db = applied.targets[1].quarantined_as.clone().unwrap();
        assert_ne!(da, db);
        assert_eq!(std::fs::read_to_string(&da).unwrap(), "aaa");
        assert_eq!(std::fs::read_to_string(&db).unwrap(), "bbb");
        restore(&preview.id).unwrap();
        assert_eq!(std::fs::read_to_string(&fa).unwrap(), "aaa");
        assert_eq!(std::fs::read_to_string(&fb).unwrap(), "bbb");
    }

    #[test]
    fn plan_rejects_duplicates_and_overlaps() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let err = plan_removal(root.path(), &[file.clone(), file]).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");

        let sub = root.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), "x").unwrap();
        let err = plan_removal(root.path(), &[sub.clone(), sub.join("inner.txt")]).unwrap_err();
        assert!(err.to_string().contains("overlapping"), "{err}");
    }

    #[test]
    fn plan_rejects_root_and_quarantine() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let err = plan_removal(root.path(), &[root.path().to_path_buf()]).unwrap_err();
        assert!(err.to_string().contains("root itself"), "{err}");

        let q = root.path().join(QUARANTINE_DIR_NAME);
        std::fs::create_dir_all(&q).unwrap();
        std::fs::write(q.join("note.txt"), "x").unwrap();
        let err = plan_removal(root.path(), &[q.join("note.txt")]).unwrap_err();
        assert!(err.to_string().contains("quarantine"), "{err}");
    }

    #[test]
    fn apply_is_idempotent_and_resumable() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let fa = root.path().join("a.txt");
        let fb = root.path().join("b.txt");
        std::fs::write(&fa, "a").unwrap();
        std::fs::write(&fb, "b").unwrap();
        let preview = plan_removal(root.path(), &[fa.clone(), fb.clone()]).unwrap();
        let first = apply_plan(&preview.id).unwrap();
        // Second apply is a verified no-op, not an error.
        let second = apply_plan(&preview.id).unwrap();
        assert_eq!(
            first.targets[0].quarantined_as,
            second.targets[0].quarantined_as
        );
        // Restore and purge are idempotent too.
        restore(&preview.id).unwrap();
        let restored_twice = restore(&preview.id).unwrap();
        assert!(restored_twice.targets.iter().all(|t| t.restored));
        assert!(fa.exists() && fb.exists());
    }

    #[test]
    fn apply_refuses_occupied_destination() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        // Squat the deterministic destination with a dangling symlink:
        // exists() would miss it, the no-replace rename must not.
        let dest = quarantine_destination(
            &load_plan(&preview.id).unwrap(),
            0,
            &load_plan(&preview.id).unwrap().targets[0].path,
        )
        .unwrap();
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(root.path().join("nowhere"), &dest).unwrap();
        let err = apply_plan(&preview.id).unwrap_err();
        assert!(format!("{err:?}").contains("refusing rename"), "{err:?}");
        assert!(file.exists(), "source must be untouched");
    }

    #[test]
    fn plan_rejects_protected_descendants() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        // A nested repository is refused by the guard.
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join("data.txt"), "x").unwrap();
        let err = plan_removal(root.path(), &[repo]).unwrap_err();
        assert!(err.to_string().contains("git repository"), "{err}");
        // An operator hold nested inside is refused by the tree walk.
        let held = root.path().join("held");
        std::fs::create_dir_all(&held).unwrap();
        std::fs::write(held.join(crate::guard::PROTECT_MARKER), "hold").unwrap();
        std::fs::write(held.join("data.txt"), "x").unwrap();
        let err = plan_removal(root.path(), &[held]).unwrap_err();
        assert!(err.to_string().contains("protected"), "{err}");
    }

    #[test]
    fn purge_refuses_protected_content_added_after_apply() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let dir = root.path().join("work");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("data.txt"), "x").unwrap();
        let preview = plan_removal(root.path(), &[dir]).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let quarantined = applied.targets[0].quarantined_as.clone().unwrap();
        // A repository appears inside the quarantined tree afterwards.
        std::fs::create_dir_all(quarantined.join(".git")).unwrap();
        let err = purge_apply(&preview.id).unwrap_err();
        assert!(err.to_string().contains("protected"), "{err}");
        assert!(quarantined.exists(), "nothing may be deleted");
    }

    #[test]
    fn describe_reviews_unapplied_plan() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let described = describe_plan(&preview.id).unwrap();
        assert_eq!(described.targets.len(), 1);
        assert!(described.targets[0].quarantined_as.is_none());
        assert!(file.exists(), "describe must not mutate");
    }
}
