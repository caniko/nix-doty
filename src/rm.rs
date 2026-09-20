//! Guarded scratch removal: plan first, quarantine on apply, restore or
//! separately-approved purge afterwards.
//!
//! Nothing here deletes user data on first pass. `rm --apply` moves guarded
//! targets into a same-filesystem quarantine by no-overwrite rename; only
//! `purge --apply` removes bytes, and only for entries whose recorded
//! identity still matches. There is no force flag: protection failures abort
//! the whole batch before any mutation.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::guard::{self, GuardedPath};

static PLAN_COUNTER: AtomicU64 = AtomicU64::new(0);

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

fn save_plan(plan: &RmPlan) -> Result<()> {
    std::fs::create_dir_all(plans_dir()).with_context(|| "cannot create plan directory")?;
    let content = serde_json::to_string_pretty(plan)?;
    // No-overwrite: a plan id collision must never clobber an existing plan.
    let path = plan_path(&plan.id);
    if path.exists() {
        anyhow::bail!("plan id collision: {}", path.display());
    }
    std::fs::write(&path, content).with_context(|| format!("cannot write {}", path.display()))
}

/// Validate targets and record a plan. Writes nothing on any failure.
pub fn plan_removal(root: &Path, targets: &[PathBuf]) -> Result<RmPreview> {
    if targets.is_empty() {
        anyhow::bail!("no removal targets given");
    }
    let mut planned = Vec::with_capacity(targets.len());
    for target in targets {
        let guarded: GuardedPath = guard::guard_path(target, root)?;
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
    let root_canonical = root
        .canonicalize()
        .with_context(|| format!("cannot resolve root {}", root.display()))?;
    let plan = RmPlan {
        id: new_plan_id(),
        created_at: chrono_now_rfc3339(),
        root: root_canonical.clone(),
        quarantine_dir: root_canonical.join(QUARANTINE_DIR_NAME),
        targets: planned,
    };
    let preview = RmPreview {
        id: plan.id.clone(),
        root: plan.root.clone(),
        targets: plan.targets.iter().map(|target| target.path.clone()).collect(),
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

/// Revalidate the whole batch, then quarantine by no-overwrite rename.
/// Any failure aborts before the first rename; partial progress after a
/// mid-batch I/O error is journaled into the plan file.
pub fn apply_plan(id: &str) -> Result<RmPlan> {
    let plan = load_plan(id)?;
    if plan.targets.iter().any(|target| target.quarantined_as.is_some()) {
        anyhow::bail!("plan {id} was already applied; restore or purge it instead");
    }
    // Whole-batch preflight: identity + policy for every target first.
    let mut fresh: Vec<(usize, GuardedPath)> = Vec::with_capacity(plan.targets.len());
    for (index, target) in plan.targets.iter().enumerate() {
        let guarded = GuardedPath {
            path: target.path.clone(),
            dev: target.dev,
            ino: target.ino,
            is_dir: target.is_dir,
        };
        let current = guard::revalidate(&guarded, &plan.root)?;
        fresh.push((index, current));
    }
    std::fs::create_dir_all(&plan.quarantine_dir).with_context(|| {
        format!(
            "cannot create quarantine {}",
            plan.quarantine_dir.display()
        )
    })?;
    // The quarantine itself is never a future removal candidate.
    let marker = plan.quarantine_dir.join(crate::guard::PROTECT_MARKER);
    if !marker.exists() {
        std::fs::write(&marker, "doty quarantine; do not remove by hand\n").with_context(|| {
            format!("cannot write {}", marker.display())
        })?;
    }

    let mut journaled = plan.clone();
    for (index, current) in &fresh {
        let target = &plan.targets[*index];
        let file_name = current
            .path
            .file_name()
            .with_context(|| format!("cannot quarantine {}", current.path.display()))?;
        let destination = plan
            .quarantine_dir
            .join(format!("{}-{}", plan.id, PathBuf::from(file_name).display()));
        if destination.exists() {
            anyhow::bail!(
                "quarantine destination exists (refusing overwrite): {}",
                destination.display()
            );
        }
        // Same-filesystem rename: quarantine lives under the allowed root,
        // whose device was verified for every target.
        std::fs::rename(&current.path, &destination).with_context(|| {
            format!(
                "cannot quarantine {} (journaled progress kept in plan {id})",
                current.path.display()
            )
        })?;
        journaled.targets[*index].quarantined_as = Some(destination);
        // Journal each move immediately so an interrupted batch is resumable
        // by inspection, never silently half-done.
        std::fs::write(
            plan_path(&journaled.id),
            serde_json::to_string_pretty(&journaled)?,
        )?;
        let _ = target;
    }
    Ok(journaled)
}

/// Move quarantined entries back. Refuses to overwrite existing paths.
pub fn restore(id: &str) -> Result<RmPlan> {
    let mut plan = load_plan(id)?;
    for target in &plan.targets {
        let Some(destination) = target.quarantined_as.as_ref() else {
            anyhow::bail!("plan {id} was never applied; nothing to restore");
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
        if target.path.exists() {
            anyhow::bail!(
                "restore destination exists (refusing overwrite): {}",
                target.path.display()
            );
        }
    }
    for index in 0..plan.targets.len() {
        let destination = plan.targets[index]
            .quarantined_as
            .clone()
            .expect("checked above");
        let id = plan.id.clone();
        std::fs::rename(&destination, &plan.targets[index].path).with_context(|| {
            format!(
                "cannot restore {} (journaled progress kept in plan {id})",
                destination.display()
            )
        })?;
        plan.targets[index].quarantined_as = None;
        plan.targets[index].restored = true;
        std::fs::write(plan_path(&id), serde_json::to_string_pretty(&plan)?)?;
    }
    Ok(plan)
}

/// Describe what `purge --apply` would remove. Read-only.
pub fn purge_preview(id: &str) -> Result<RmPlan> {
    let plan = load_plan(id)?;
    if plan.targets.iter().any(|target| target.quarantined_as.is_none()) {
        anyhow::bail!("plan {id} is not fully quarantined; purge needs an applied plan");
    }
    Ok(plan)
}

/// Permanently remove quarantined entries after revalidating identity.
/// This is the only function in doty that deletes approved scratch data,
/// and it requires a fully quarantined plan.
pub fn purge_apply(id: &str) -> Result<RmPlan> {
    let mut plan = purge_preview(id)?;
    for index in 0..plan.targets.len() {
        let destination = plan.targets[index]
            .quarantined_as
            .clone()
            .expect("checked above");
        let id = plan.id.clone();
        let expected = plan.targets[index].clone();
        let meta = std::fs::symlink_metadata(&destination)
            .with_context(|| format!("quarantined entry missing: {}", destination.display()))?;
        if meta.dev() != expected.dev || meta.ino() != expected.ino {
            anyhow::bail!(
                "quarantined entry identity changed: {}",
                destination.display()
            );
        }
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "refusing symlink at purge time: {}",
                destination.display()
            );
        }
        if meta.is_dir() {
            std::fs::remove_dir_all(&destination)
                .with_context(|| format!("cannot purge {}", destination.display()))?;
        } else {
            std::fs::remove_file(&destination)
                .with_context(|| format!("cannot purge {}", destination.display()))?;
        }
        plan.targets[index].quarantined_as = None;
        plan.targets[index].purged = true;
        std::fs::write(plan_path(&id), serde_json::to_string_pretty(&plan)?)?;
    }
    Ok(plan)
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
        assert!(err.to_string().contains("not fully quarantined"), "{err}");
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
}
