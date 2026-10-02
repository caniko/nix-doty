//! Guarded scratch removal: plan first, quarantine on apply, restore or
//! separately-approved purge afterwards.
//!
//! Nothing here deletes user data on first pass. `rm --apply` moves guarded
//! targets into a same-filesystem quarantine by atomic no-replace rename;
//! only `purge --apply` removes bytes, and only for entries whose recorded
//! identity still matches. There is no force flag: protection failures abort
//! the whole batch before any mutation.
//!
//! Recovery model: intent is journaled BEFORE each mutation
//! (`pending_op`), the journal write is crash-safe (tmp + fsync + atomic
//! rename), affected parent directories are synced before the completion
//! journal, and apply/restore/purge reconcile interrupted entries on the
//! next run instead of stranding the plan. A per-plan exclusive lock
//! serializes concurrent commands against the same journal.
//!
//! Honesty notes: process crashes are recoverable by evidence; power loss
//! is best-effort ordered, not proven. Mutations run through no-follow
//! directory handles walked from the verified root (`openat` per component,
//! `renameat2` against the resulting dirfds), the quarantine is created
//! with `mkdirat` off the root handle, and purges empty directories
//! handle-relative without ever traversing a symlink — a planted link
//! removes only itself. Residual races: the root path itself is rechecked
//! (not held open across the whole batch), and the plan-state directory
//! under $HOME is created path-relative (multi-level, no verified ancestor
//! to walk from). Do not run this against a live adversary racing every
//! syscall — it is built for accidents, not attacks.

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PendingOp {
    Quarantine,
    Restore,
    Purge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTarget {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub is_dir: bool,
    /// Size and mtime at plan time (see [`GuardedPath`]: inodes recycle).
    /// Defaults keep plans written before this field existed loadable;
    /// such plans compare size/mtime 0 and fail closed on first use.
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime_secs: i64,
    #[serde(default)]
    pub mtime_nanos: u32,
    /// Mutation intent recorded BEFORE the filesystem is touched. If a
    /// crash lands between intent and the post-mutation journal, the next
    /// run reconciles source/destination identity instead of stranding.
    #[serde(default)]
    pub pending_op: Option<PendingOp>,
    #[serde(default)]
    pub quarantined_as: Option<PathBuf>,
    #[serde(default)]
    pub restored: bool,
    #[serde(default)]
    pub purged: bool,
}

fn recorded_guard(target: &PlannedTarget) -> GuardedPath {
    GuardedPath {
        path: target.path.clone(),
        dev: target.dev,
        ino: target.ino,
        is_dir: target.is_dir,
        size: target.size,
        mtime_secs: target.mtime_secs,
        mtime_nanos: target.mtime_nanos,
    }
}

fn metadata_matches(meta: &std::fs::Metadata, target: &PlannedTarget) -> bool {
    let (dev, ino, is_dir, size, mtime_secs, mtime_nanos) = guard::identity_of(meta);
    guard::same_identity(
        dev,
        ino,
        is_dir,
        size,
        mtime_secs,
        mtime_nanos,
        &recorded_guard(target),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RmPlan {
    pub id: String,
    pub created_at: String,
    pub root: PathBuf,
    pub quarantine_dir: PathBuf,
    pub targets: Vec<PlannedTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scratch_ledger: Option<crate::scratch_ledger::Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scratch_intelligence: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RmPreview {
    pub id: String,
    pub root: PathBuf,
    pub targets: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scratch_intelligence: Option<serde_json::Value>,
}

fn plan_path(id: &str) -> PathBuf {
    plans_dir().join(format!("{id}.json"))
}

fn check_plan_id(id: &str) -> Result<()> {
    if id.contains('/') || id.contains('\0') || id.starts_with('.') {
        anyhow::bail!("invalid plan id: {id}");
    }
    Ok(())
}

pub(crate) fn plan_lock_path(id: &str) -> Result<PathBuf> {
    check_plan_id(id)?;
    Ok(plans_dir().join(format!(".{id}.lock")))
}

/// Exclusive non-blocking per-plan lock. Returned handle must be held for
/// the whole mutating operation; closing it releases the lock. Two doty
/// commands racing on one plan would otherwise overwrite each other's
/// journal and double-move entries.
fn lock_plan(id: &str) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let path = plan_lock_path(id)?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open plan lock {}", path.display()))?;
    // SAFETY: fd is a valid open file; flock takes no pointers.
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        anyhow::bail!("plan {id} is locked by another process; retry when it finishes");
    }
    Ok(file)
}

fn load_plan(id: &str) -> Result<RmPlan> {
    check_plan_id(id)?;
    let content = std::fs::read_to_string(plan_path(id))
        .with_context(|| format!("unknown removal plan: {id}"))?;
    serde_json::from_str(&content).with_context(|| format!("cannot parse plan {id}"))
}

/// Private directory: created if missing, permissions forced to 0700.
///
/// The chmod goes through a no-follow directory handle, never the path:
/// a symlink swapped in between `create_dir_all` and the chmod would
/// otherwise redirect permissions (or creation) outside the root.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create directory {}", dir.display()))?;
    let handle = open_dir_nofollow(dir)?;
    handle
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict {}", dir.display()))?;
    Ok(())
}

/// Open a directory without following a trailing symlink. Fails when the
/// path is a link, is not a directory, or cannot be opened.
fn open_dir_nofollow(dir: &Path) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;
    let dir_c = CString::new(dir.as_os_str().as_bytes())
        .with_context(|| format!("cannot encode {}", dir.display()))?;
    // SAFETY: valid NUL-terminated C string borrowed for the call;
    // O_NOFOLLOW refuses a trailing symlink; O_DIRECTORY refuses non-dirs.
    let fd = unsafe {
        libc::open(
            dir_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!(
            "refusing directory that is not a real dir {}: {err}",
            dir.display()
        );
    }
    // SAFETY: fd is a fresh owned descriptor from open above.
    let handle = unsafe { std::fs::File::from_raw_fd(fd) };
    let stat: libc::stat = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut stat) != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("cannot fstat {}: {err}", dir.display());
        }
        stat
    };
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        anyhow::bail!("refusing non-directory {}", dir.display());
    }
    Ok(handle)
}

/// fstat through an open handle. Reads the opened object, never the path.
fn fstat_of(handle: &std::fs::File, what: &str) -> Result<libc::stat> {
    use std::os::fd::AsRawFd;
    unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(handle.as_raw_fd(), &mut stat) != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("cannot fstat {what}: {err}");
        }
        Ok(stat)
    }
}

/// lstat through an open directory handle: classifies a child without
/// opening it, so FIFOs and other special files can never block us and a
/// trailing symlink is reported as a link, never traversed.
fn lstat_at(parent: &std::fs::File, name: &std::ffi::OsStr) -> Result<libc::stat> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name_c = CString::new(name.as_bytes())
        .with_context(|| format!("cannot encode {}", name.to_string_lossy()))?;
    // SAFETY: name is a single path component borrowed for the call;
    // AT_SYMLINK_NOFOLLOW stats the link itself.
    unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstatat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        ) != 0
        {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("cannot stat {}: {err}", name.to_string_lossy());
        }
        Ok(stat)
    }
}

/// Open a single child component of an open directory without following a
/// trailing symlink. Extra open flags (e.g. O_DIRECTORY) further constrain
/// what the child may be.
fn open_at_nofollow(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    access: libc::c_int,
) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;
    let name_c = CString::new(name.as_bytes())
        .with_context(|| format!("cannot encode {}", name.to_string_lossy()))?;
    // SAFETY: name is a single path component borrowed for the call.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            access | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("cannot open {}: {err}", name.to_string_lossy());
    }
    // SAFETY: fd is a fresh owned descriptor from openat above.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Fsync an open directory handle's directory so completed renames/unlinks
/// are durable before any journal claims them.
fn sync_dir_handle(dir: &std::fs::File, what: &str) -> Result<()> {
    dir.sync_all()
        .with_context(|| format!("cannot sync {what}"))?;
    Ok(())
}

/// Fsync a directory by path (plan/state dirs with no verified handle).
fn sync_dir(dir: &Path) -> Result<()> {
    let handle =
        std::fs::File::open(dir).with_context(|| format!("cannot open {}", dir.display()))?;
    sync_dir_handle(&handle, &dir.display().to_string())
}

/// Fsync the parent directory so a completed rename/unlink is durable
/// before the journal claims it. Crash model: process crashes are fully
/// recoverable via intent + reconcile. Power loss is best-effort ordered
/// (intent → mutation → parent sync → completion journal); surviving
/// journals of lost mutations reconcile by evidence, never by assumption.
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            sync_dir(parent)?;
        }
    }
    Ok(())
}

/// Crash-safe journal write: temp file + fsync + atomic rename + dir fsync.
/// A crash can never leave a truncated journal behind.
fn journal_write(plan: &RmPlan) -> Result<()> {
    let dir = plans_dir();
    ensure_private_dir(&dir)?;
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

/// Open the parent directory of `path` by walking components from an
/// already-verified `root` handle, one no-follow step at a time. Each
/// intermediate component is opened with O_NOFOLLOW|O_DIRECTORY, so a
/// symlink swapped in after planning cannot redirect the walk: the openat
/// fails instead of traversing the link.
fn parent_fd_under_root(
    root: &std::fs::File,
    root_path: &Path,
    path: &Path,
) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;
    let parent = path
        .parent()
        .with_context(|| format!("path {} has no parent", path.display()))?;
    let rel = parent.strip_prefix(root_path).with_context(|| {
        format!(
            "path {} is not under root {}",
            path.display(),
            root_path.display()
        )
    })?;
    // SAFETY: dup gives an owned descriptor for the walk cursor.
    let mut cursor = unsafe {
        let duped = libc::dup(root.as_raw_fd());
        if duped < 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("cannot dup root handle: {err}");
        }
        std::fs::File::from_raw_fd(duped)
    };
    for component in rel.components() {
        let name = component.as_os_str().as_bytes();
        let name_c =
            CString::new(name).with_context(|| format!("cannot encode {}", parent.display()))?;
        // SAFETY: name is a single path component (no slashes) borrowed for
        // the call; O_NOFOLLOW|O_DIRECTORY refuse links and non-dirs.
        let fd = unsafe {
            libc::openat(
                cursor.as_raw_fd(),
                name_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!(
                "refusing path with unreachable ancestor {}: {err}",
                parent.display()
            );
        }
        // SAFETY: fd is a fresh owned descriptor from openat above.
        cursor = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    Ok(cursor)
}

/// Contained rename: both endpoints are ancestry-checked, then the rename
/// runs relative to no-follow parent handles walked from the verified root.
/// The kernel resolves only the final basenames against those handles, so a
/// hostile ancestor swapped in after planning fails the walk instead of
/// redirecting the move. (A swap of the root path itself is still refused by
/// [`assert_ancestry`]'s root check.)
fn rename_contained(root: &Path, from: &Path, to: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    assert_ancestry(from, root)?;
    assert_ancestry(to, root)?;
    let root_handle = open_dir_nofollow(root)?;
    let from_parent = parent_fd_under_root(&root_handle, root, from)?;
    let to_parent = parent_fd_under_root(&root_handle, root, to)?;
    let base = |p: &Path| -> Result<CString> {
        let name = p
            .file_name()
            .with_context(|| format!("cannot name {}", p.display()))?;
        CString::new(name.as_bytes()).with_context(|| format!("cannot encode {}", p.display()))
    };
    let from_c = base(from)?;
    let to_c = base(to)?;
    // SAFETY: pointers are valid NUL-terminated basenames borrowed for the
    // call; dirfds are verified directory handles; RENAME_NOREPLACE is a
    // flag-only argument with no pointer semantics.
    let ret = unsafe {
        libc::renameat2(
            from_parent.as_raw_fd(),
            from_c.as_ptr(),
            to_parent.as_raw_fd(),
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

/// Unlink one child name relative to an open directory handle.
fn unlink_at(parent: &std::fs::File, name: &std::ffi::OsStr, is_dir: bool) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name_c = CString::new(name.as_bytes()).with_context(|| "cannot encode entry name")?;
    let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: name is a single component borrowed for the call; unlinkat
    // never follows a trailing symlink — it removes the link itself.
    let ret = unsafe { libc::unlinkat(parent.as_raw_fd(), name_c.as_ptr(), flags) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("cannot remove entry: {err}");
    }
    Ok(())
}

/// Policy sweep before any deletion: walk the whole tree refusing
/// protection-marker names and device boundaries. Name checks need no
/// opens at all; subdirectories are opened O_DIRECTORY (never blocking).
/// A marker present anywhere aborts with nothing deleted. A concurrent
/// writer planting one mid-sweep can still interleave — this is a
/// deterministic gate, not transactional protection.
fn policy_tree_fd(dir: &std::fs::File, expected_dev: u64) -> Result<()> {
    use std::os::fd::AsRawFd;
    let listing = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let entries =
        std::fs::read_dir(&listing).with_context(|| "cannot list quarantine directory")?;
    for entry in entries {
        let entry = entry.with_context(|| "cannot read quarantine entry")?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        if name == ".git" || name == crate::guard::PROTECT_MARKER {
            anyhow::bail!(
                "refusing protected content inside quarantine: {}",
                name.to_string_lossy()
            );
        }
        let stat = lstat_at(dir, &name).with_context(|| {
            format!(
                "cannot inspect {}; refusing to guess",
                name.to_string_lossy()
            )
        })?;
        if stat.st_dev != expected_dev as libc::dev_t {
            anyhow::bail!(
                "refusing mount boundary inside quarantine: {}",
                name.to_string_lossy()
            );
        }
        if stat.st_mode as libc::mode_t & libc::S_IFMT == libc::S_IFDIR {
            let child = open_at_nofollow(dir, &name, libc::O_RDONLY | libc::O_DIRECTORY)
                .with_context(|| {
                    format!("cannot open {}; refusing to guess", name.to_string_lossy())
                })?;
            let open_stat = fstat_of(&child, "quarantine entry")?;
            if open_stat.st_mode as libc::mode_t & libc::S_IFMT != libc::S_IFDIR
                || open_stat.st_dev != stat.st_dev
                || open_stat.st_ino != stat.st_ino
            {
                anyhow::bail!(
                    "quarantine entry changed under us {}; refusing to guess",
                    name.to_string_lossy()
                );
            }
            policy_tree_fd(&child, expected_dev)?;
        }
    }
    Ok(())
}

/// Empty a directory through its open handle, never traversing a symlink
/// and never opening a child for classification: each child is typed with
/// lstat (no FIFO can block us), links are unlinked as links, and only
/// verified directories are opened for recursion. Any inspection error
/// fails closed without deleting anything.
///
/// Protection policy is rechecked during traversal, not just at preflight:
/// a policy sweep runs before the first unlink, so a marker or mount
/// present anywhere aborts with nothing deleted. A concurrent writer
/// racing the sweep itself can still interleave — this narrows the window
/// to the traversal, it does not close it.
fn remove_tree_fd(dir: &std::fs::File, expected_dev: u64) -> Result<()> {
    use std::os::fd::AsRawFd;
    policy_tree_fd(dir, expected_dev)?;
    let listing = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let entries =
        std::fs::read_dir(&listing).with_context(|| "cannot list quarantine directory")?;
    for entry in entries {
        let entry = entry.with_context(|| "cannot read quarantine entry")?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        if name == ".git" || name == crate::guard::PROTECT_MARKER {
            anyhow::bail!(
                "refusing protected content inside quarantine: {}",
                name.to_string_lossy()
            );
        }
        let stat = lstat_at(dir, &name).with_context(|| {
            format!(
                "cannot inspect {}; refusing to guess",
                name.to_string_lossy()
            )
        })?;
        if stat.st_dev != expected_dev as libc::dev_t {
            anyhow::bail!(
                "refusing mount boundary inside quarantine: {}",
                name.to_string_lossy()
            );
        }
        match stat.st_mode as libc::mode_t & libc::S_IFMT {
            libc::S_IFDIR => {
                // O_DIRECTORY: a swap for a FIFO after the lstat fails
                // instead of blocking.
                let child = open_at_nofollow(dir, &name, libc::O_RDONLY | libc::O_DIRECTORY)
                    .with_context(|| {
                        format!("cannot open {}; refusing to guess", name.to_string_lossy())
                    })?;
                let open_stat = fstat_of(&child, "quarantine entry")?;
                if open_stat.st_mode as libc::mode_t & libc::S_IFMT != libc::S_IFDIR
                    || open_stat.st_dev != stat.st_dev
                    || open_stat.st_ino != stat.st_ino
                {
                    anyhow::bail!(
                        "quarantine entry changed under us {}; refusing to guess",
                        name.to_string_lossy()
                    );
                }
                remove_tree_fd(&child, expected_dev)?;
                unlink_at(dir, &name, true)?;
            }
            // Files, FIFOs, sockets, devices, and links: unlink the name
            // itself. Nothing here is ever opened, so nothing can block.
            _ => {
                unlink_at(dir, &name, false)?;
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

/// Every ancestor from `path` up to (excluding) `root` must be a real
/// directory: no symlinks, no missing components. Recorded canonical paths
/// are safe at plan time, but ancestors can be swapped afterwards and a
/// rename through a hostile ancestor would land outside the approved root.
fn assert_ancestry(path: &Path, root: &Path) -> Result<()> {
    // The root itself must be a real directory: it was canonical when
    // recorded, but the path may have been swapped for a link since.
    let root_meta = std::fs::symlink_metadata(root)
        .with_context(|| format!("cannot stat root {}", root.display()))?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        anyhow::bail!(
            "refusing operation under a root that is not a real directory: {}",
            root.display()
        );
    }
    // Start at the parent: the entry itself may legitimately be absent
    // (it is in quarantine while we validate its restore path).
    let Some(parent) = path.parent() else {
        anyhow::bail!("path {} has no parent", path.display());
    };
    for ancestor in parent.ancestors() {
        if ancestor == root {
            return Ok(());
        }
        let meta = std::fs::symlink_metadata(ancestor).with_context(|| {
            format!(
                "refusing path with unreachable ancestor {}",
                ancestor.display()
            )
        })?;
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "refusing path through symlinked ancestor {}",
                ancestor.display()
            );
        }
        if !meta.is_dir() {
            anyhow::bail!(
                "refusing path through non-directory ancestor {}",
                ancestor.display()
            );
        }
    }
    anyhow::bail!(
        "path {} is not under root {}",
        path.display(),
        root.display()
    )
}

/// Prepare the quarantine directory defensively, relative to a no-follow
/// handle on the root: `mkdirat` + `openat(O_NOFOLLOW)` never trust the
/// path. A symlink swapped in around creation fails the open instead of
/// redirecting the quarantine (or its permissions/marker) off-root.
fn prepare_quarantine(plan: &RmPlan) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;
    let root_handle = open_dir_nofollow(&plan.root)?;
    let name = CString::new(QUARANTINE_DIR_NAME.as_bytes())
        .with_context(|| "cannot encode quarantine name")?;
    // SAFETY: name is a single component borrowed for the call.
    let ret = unsafe { libc::mkdirat(root_handle.as_raw_fd(), name.as_ptr(), 0o700) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::AlreadyExists {
            anyhow::bail!(
                "cannot create quarantine {}: {err}",
                plan.quarantine_dir.display()
            );
        }
    }
    // SAFETY: name is a single component borrowed for the call;
    // O_NOFOLLOW refuses a swapped-in link, O_DIRECTORY refuses non-dirs.
    let fd = unsafe {
        libc::openat(
            root_handle.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!(
            "refusing quarantine that is not a real directory {}: {err}",
            plan.quarantine_dir.display()
        );
    }
    use std::os::unix::io::FromRawFd;
    // SAFETY: fd is a fresh owned descriptor from openat above.
    let handle = unsafe { std::fs::File::from_raw_fd(fd) };
    handle
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict {}", plan.quarantine_dir.display()))?;
    let root_dev = std::fs::metadata(&plan.root)
        .with_context(|| format!("cannot stat root {}", plan.root.display()))?
        .dev();
    let stat: libc::stat = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(handle.as_raw_fd(), &mut stat) != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("cannot fstat quarantine: {err}");
        }
        stat
    };
    if stat.st_dev != root_dev as libc::dev_t {
        anyhow::bail!(
            "refusing quarantine on another device: {}",
            plan.quarantine_dir.display()
        );
    }
    // The quarantine itself is never a future removal candidate. Created
    // exclusively through the verified handle: no path-based check/use gap.
    let marker = CString::new(crate::guard::PROTECT_MARKER.as_bytes())
        .with_context(|| "cannot encode marker name")?;
    // SAFETY: marker is a single component borrowed for the call; O_EXCL
    // fails when the marker already exists, O_NOFOLLOW refuses a link.
    let mfd = unsafe {
        libc::openat(
            handle.as_raw_fd(),
            marker.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if mfd < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::AlreadyExists {
            anyhow::bail!(
                "cannot write quarantine marker {}: {err}",
                plan.quarantine_dir.display()
            );
        }
        // Marker exists: classify it with lstat before any open — a FIFO
        // marker must be refused, never opened (a blocking open here
        // would hang under the plan lock).
        let marker_os = std::ffi::OsStr::new(crate::guard::PROTECT_MARKER);
        let marker_stat = lstat_at(&handle, marker_os)?;
        if marker_stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            anyhow::bail!(
                "refusing quarantine with unexpected marker: {}",
                plan.quarantine_dir.display()
            );
        }
    } else {
        use std::io::Write;
        // SAFETY: mfd is a fresh owned descriptor from openat above.
        let mut file = unsafe { std::fs::File::from_raw_fd(mfd) };
        file.write_all(b"doty quarantine; do not remove by hand\n")
            .with_context(|| {
                format!(
                    "cannot write quarantine marker {}",
                    plan.quarantine_dir.display()
                )
            })?;
    }
    Ok(())
}

/// The quarantine directory must be a real directory on the root device
/// every time it is trusted (reconcile, preflight, deletion). Creation is
/// separate ([`prepare_quarantine`]); this is the no-create check used on
/// paths that must already be intact.
fn assert_quarantine_intact(plan: &RmPlan) -> Result<()> {
    // No-follow handle: a swapped-in symlink fails the open, and the device
    // check below reads the opened directory, not the path.
    let handle = open_dir_nofollow(&plan.quarantine_dir).with_context(|| {
        format!(
            "quarantine missing or unreachable: {}",
            plan.quarantine_dir.display()
        )
    })?;
    let root_dev = std::fs::metadata(&plan.root)
        .with_context(|| format!("cannot stat root {}", plan.root.display()))?
        .dev();
    let stat: libc::stat = unsafe {
        use std::os::fd::AsRawFd;
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(handle.as_raw_fd(), &mut stat) != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("cannot fstat quarantine: {err}");
        }
        stat
    };
    if stat.st_dev != root_dev as libc::dev_t {
        anyhow::bail!(
            "refusing quarantine on another device: {}",
            plan.quarantine_dir.display()
        );
    }
    Ok(())
}
/// Reconcile entries left with a recorded intent by an interrupted run.
/// Returns true when the journal was updated.
///
/// Every case is decided by identity evidence, never by assumption: a
/// matching source means the mutation never ran (clear intent, retry); a
/// matching mutation product means the mutation ran but the journal was
/// lost (record the outcome); anything else means the object is
/// unaccounted for (fail closed).
fn reconcile_pending(plan: &mut RmPlan) -> Result<bool> {
    if !plan.targets.iter().any(|t| t.pending_op.is_some()) {
        return Ok(false);
    }
    // Destinations live inside the quarantine; if it is compromised or
    // unreachable, no absence/presence inference is sound. The one
    // exception is a quarantine that was never created: only Quarantine
    // intents are satisfiable then (they create it via prepare_quarantine);
    // Restore/Purge intents reference objects that cannot exist.
    match std::fs::symlink_metadata(&plan.quarantine_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if plan.targets.iter().any(|t| {
                matches!(
                    t.pending_op,
                    Some(PendingOp::Restore) | Some(PendingOp::Purge)
                )
            }) {
                anyhow::bail!(
                    "plan {} has restore/purge intent but no quarantine exists; refusing to guess",
                    plan.id
                );
            }
        }
        // An unreadable quarantine (permissions, I/O) fails here before any
        // per-entry inference: absence/presence evidence cannot be trusted,
        // so every intent is preserved by construction.
        _ => assert_quarantine_intact(plan).with_context(|| {
            format!(
                "plan {} has pending intent but the quarantine is unreachable; intent preserved",
                plan.id
            )
        })?,
    }
    let mut changed = false;
    // Directories whose dirents a recovered mutation changed. Collected per
    // outcome — never derived from post-recovery state — so completion is
    // synced exactly where it happened: the quarantine dir for every
    // completed move in or out of it, plus the source/restore parent only
    // when that side actually changed. A "never ran" clearing syncs
    // nothing; a purge never touches the original parent.
    let mut sync_dirs: Vec<PathBuf> = Vec::new();
    let note_sync = |dirs: &mut Vec<PathBuf>, dir: PathBuf| {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    for index in 0..plan.targets.len() {
        let op = match plan.targets[index].pending_op {
            None => continue,
            Some(op) => op,
        };
        let target = plan.targets[index].clone();
        match op {
            PendingOp::Quarantine => {
                let destination = quarantine_destination(plan, index, &target.path)?;
                let recorded = recorded_guard(&target);
                let source_ok = guard::revalidate(&recorded, &plan.root).is_ok();
                if source_ok {
                    plan.targets[index].pending_op = None;
                    changed = true;
                    continue;
                }
                match std::fs::symlink_metadata(&destination) {
                    Ok(meta) if metadata_matches(&meta, &target) => {
                        plan.targets[index].quarantined_as = Some(destination);
                        plan.targets[index].pending_op = None;
                        if let Some(parent) = target.path.parent() {
                            note_sync(&mut sync_dirs, parent.to_path_buf());
                        }
                        note_sync(&mut sync_dirs, plan.quarantine_dir.clone());
                        changed = true;
                    }
                    _ => anyhow::bail!(
                        "plan {} entry {} has recorded quarantine intent but neither source nor destination matches; refusing to guess",
                        plan.id,
                        target.path.display()
                    ),
                }
            }
            PendingOp::Restore => {
                let Some(destination) = target.quarantined_as.clone() else {
                    anyhow::bail!(
                        "plan {} entry {} has restore intent without a quarantine record; refusing to guess",
                        plan.id,
                        target.path.display()
                    );
                };
                match std::fs::symlink_metadata(&destination) {
                    Ok(meta) if metadata_matches(&meta, &target) => {
                        plan.targets[index].pending_op = None;
                        changed = true;
                        continue;
                    }
                    _ => {}
                }
                // The quarantine object is gone; if an identical object now
                // sits at the restore path, the rename ran. (For files the
                // identity is byte-exact in practice; for dirs the vanished
                // quarantine object plus an identical one at the recorded
                // path is the only consistent crash story.)
                let back = std::fs::symlink_metadata(&target.path).ok().filter(|meta| {
                    !meta.file_type().is_symlink() && metadata_matches(meta, &target)
                });
                if back.is_some() {
                    plan.targets[index].quarantined_as = None;
                    plan.targets[index].restored = true;
                    plan.targets[index].pending_op = None;
                    if let Some(parent) = target.path.parent() {
                        note_sync(&mut sync_dirs, parent.to_path_buf());
                    }
                    note_sync(&mut sync_dirs, plan.quarantine_dir.clone());
                    changed = true;
                } else {
                    anyhow::bail!(
                        "plan {} entry {} has restore intent but neither quarantine nor restore path matches; refusing to guess",
                        plan.id,
                        target.path.display()
                    );
                }
            }
            PendingOp::Purge => {
                let Some(destination) = target.quarantined_as.clone() else {
                    anyhow::bail!(
                        "plan {} entry {} has purge intent without a quarantine record; refusing to guess",
                        plan.id,
                        target.path.display()
                    );
                };
                match std::fs::symlink_metadata(&destination) {
                    Ok(meta) if metadata_matches(&meta, &target) => {
                        plan.targets[index].pending_op = None;
                        changed = true;
                    }
                    Ok(_) => anyhow::bail!(
                        "plan {} entry {} has purge intent but the quarantined object changed; refusing to guess",
                        plan.id,
                        destination.display()
                    ),
                    // Only absence counts as deletion evidence — and only
                    // with an intact quarantine (checked above). Any other
                    // error (permissions, I/O) preserves the intent and
                    // fails: the bytes may still exist.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        plan.targets[index].quarantined_as = None;
                        plan.targets[index].purged = true;
                        plan.targets[index].pending_op = None;
                        // The only dirent change is inside the quarantine;
                        // the original parent is untouched by a purge.
                        note_sync(&mut sync_dirs, plan.quarantine_dir.clone());
                        changed = true;
                    }
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!(
                                "plan {} entry {} has purge intent but the quarantine is unreadable; intent preserved",
                                plan.id,
                                destination.display()
                            )
                        });
                    }
                }
            }
        }
    }
    if changed {
        // Persisting a recovered outcome is itself a completion claim: sync
        // exactly the directories the recovered mutations touched — as
        // recorded above, not re-derived from cleared state — so the
        // journal never runs ahead of filesystem durability and never
        // demands unrelated directories to exist.
        for dir in &sync_dirs {
            sync_dir(dir)?;
        }
        journal_write(plan)?;
    }
    Ok(changed)
}

/// Validate targets and record a plan. Writes nothing on any failure.
pub fn plan_removal(root: &Path, targets: &[PathBuf]) -> Result<RmPreview> {
    plan_removal_with_ledger(root, targets, None)
}

pub(crate) fn plan_removal_with_ledger(
    root: &Path,
    targets: &[PathBuf],
    ledger: Option<crate::scratch_ledger::Config>,
) -> Result<RmPreview> {
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
            size: guarded.size,
            mtime_secs: guarded.mtime_secs,
            mtime_nanos: guarded.mtime_nanos,
            pending_op: None,
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
    let scratch_intelligence = if let Some(config) = &ledger {
        config.validate_root(&root_canonical)?;
        let paths = planned.iter().map(|t| t.path.clone()).collect::<Vec<_>>();
        Some(config.inspect_for_cleanup(&crate::scratch_ledger::paths(&paths))?)
    } else {
        None
    };
    let plan = RmPlan {
        id: new_plan_id(),
        created_at: chrono_now_rfc3339(),
        root: root_canonical,
        quarantine_dir,
        targets: planned,
        scratch_ledger: ledger,
        scratch_intelligence,
    };
    let preview = RmPreview {
        id: plan.id.clone(),
        root: plan.root.clone(),
        targets: plan
            .targets
            .iter()
            .map(|target| target.path.clone())
            .collect(),
        scratch_intelligence: plan.scratch_intelligence.clone(),
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
    let _lock = lock_plan(id)?;
    let mut journaled = load_plan(id)?;
    reconcile_pending(&mut journaled)?;
    let plan = journaled.clone();
    // Whole-batch preflight for pending entries first: identity + policy.
    // A resume trusts recorded quarantine locations, so the quarantine must
    // be intact before any of them are read.
    if plan.targets.iter().any(|t| t.quarantined_as.is_some()) {
        assert_quarantine_intact(&plan)?;
    }
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
            if !metadata_matches(&meta, target) {
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
        let guarded = recorded_guard(target);
        guard::revalidate(&guarded, &plan.root)?;
        pending.push(index);
    }
    if pending.is_empty() {
        return Ok(journaled);
    }
    if let Some(ledger) = &plan.scratch_ledger {
        let paths = pending
            .iter()
            .map(|&i| plan.targets[i].path.clone())
            .collect::<Vec<_>>();
        journaled.scratch_intelligence =
            Some(ledger.require_released(&crate::scratch_ledger::paths(&paths))?);
    }
    prepare_quarantine(&plan)?;

    for index in pending {
        let target = &plan.targets[index];
        let destination = quarantine_destination(&plan, index, &target.path)?;
        if let Some(ledger) = &plan.scratch_ledger {
            ledger.require_released(&crate::scratch_ledger::paths(std::slice::from_ref(
                &target.path,
            )))?;
        }
        journaled.targets[index].pending_op = Some(PendingOp::Quarantine);
        journal_write(&journaled)?;
        rename_contained(&plan.root, &target.path, &destination).with_context(|| {
            format!(
                "cannot quarantine {} (journaled progress kept in plan {id})",
                target.path.display()
            )
        })?;
        sync_parent(&target.path)?;
        sync_parent(&destination)?;
        journaled.targets[index].quarantined_as = Some(destination);
        journaled.targets[index].pending_op = None;
        // Journal each move immediately so an interrupted batch is resumable
        // by inspection, never silently half-done.
        journal_write(&journaled)?;
    }
    Ok(journaled)
}

/// Move quarantined entries back. Already-restored entries are skipped so
/// interrupted restores resume; refuses to overwrite existing paths.
/// Entries that were never applied are left untouched (a partially applied
/// batch restores its quarantined subset); use `describe` to review entry
/// states. Restoring a fully unapplied plan is a no-op.
pub fn restore(id: &str) -> Result<RmPlan> {
    let _lock = lock_plan(id)?;
    let mut journaled = load_plan(id)?;
    reconcile_pending(&mut journaled)?;
    let plan = journaled.clone();
    // Two-phase: verify every pending restore before moving the first.
    let mut pending: Vec<usize> = Vec::new();
    if plan.targets.iter().any(|t| t.quarantined_as.is_some()) {
        assert_quarantine_intact(&plan)?;
    }
    for (index, target) in plan.targets.iter().enumerate() {
        let Some(destination) = target.quarantined_as.as_ref() else {
            // Never applied (or already restored): not ours to move.
            continue;
        };
        // Ancestry first: the recorded path is only safe if every ancestor
        // is still a real directory under the root.
        assert_ancestry(&target.path, &plan.root)?;
        // The quarantined entry must still be the same object we moved.
        let meta = std::fs::symlink_metadata(destination)
            .with_context(|| format!("quarantined entry missing: {}", destination.display()))?;
        if !metadata_matches(&meta, target) {
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
        journaled.targets[index].pending_op = Some(PendingOp::Restore);
        journal_write(&journaled)?;
        rename_contained(&plan.root, &destination, &journaled.targets[index].path).with_context(
            || {
                format!(
                    "cannot restore {} (journaled progress kept in plan {id})",
                    destination.display()
                )
            },
        )?;
        sync_parent(&destination)?;
        sync_parent(&journaled.targets[index].path)?;
        journaled.targets[index].quarantined_as = None;
        journaled.targets[index].restored = true;
        journaled.targets[index].pending_op = None;
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
    let _lock = lock_plan(id)?;
    let mut journaled = load_plan(id)?;
    reconcile_pending(&mut journaled)?;
    let plan = journaled.clone();
    // Two-phase: verify every pending purge before deleting the first.
    let mut pending: Vec<usize> = Vec::new();
    if plan.targets.iter().any(|t| t.quarantined_as.is_some()) {
        assert_quarantine_intact(&plan)?;
    }
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
        if !metadata_matches(&meta, target) {
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
    if let Some(ledger) = &plan.scratch_ledger {
        let paths = pending
            .iter()
            .map(|&i| crate::scratch_ledger::QueryPath {
                path: plan.targets[i].path.clone(),
                at: plan.targets[i].quarantined_as.clone(),
            })
            .collect::<Vec<_>>();
        journaled.scratch_intelligence = Some(ledger.require_released(&paths)?);
    }
    for index in pending {
        let destination = journaled.targets[index]
            .quarantined_as
            .clone()
            .expect("checked above");
        let expected = journaled.targets[index].clone();
        if let Some(ledger) = &plan.scratch_ledger {
            ledger.require_released(&[crate::scratch_ledger::QueryPath {
                path: expected.path.clone(),
                at: Some(destination.clone()),
            }])?;
        }
        // Re-check identity at deletion time: the preflight above and this
        // check bracket the batch, but entries are removed one by one.
        let meta = std::fs::symlink_metadata(&destination)
            .with_context(|| format!("quarantined entry missing: {}", destination.display()))?;
        if !metadata_matches(&meta, &expected) {
            anyhow::bail!(
                "quarantined entry identity changed: {}",
                destination.display()
            );
        }
        if meta.file_type().is_symlink() {
            anyhow::bail!("refusing symlink at purge time: {}", destination.display());
        }
        // Deletion-time tree revalidation: entries are removed one by one,
        // so a directory's contents are re-checked immediately before its
        // own removal, bracketing the preflight above as tightly as possible.
        if meta.is_dir() {
            validate_tree(&destination, expected.dev)?;
        }
        journaled.targets[index].pending_op = Some(PendingOp::Purge);
        journal_write(&journaled)?;
        // Handle-relative removal: the quarantine is reopened no-follow at
        // deletion time (closing the preflight-to-deletion window), and the
        // entry is classified with lstat — never opened — so a FIFO can
        // never block us. Directories are emptied without ever traversing
        // a symlink; a planted link removes only itself.
        let quarantine_handle = open_dir_nofollow(&journaled.quarantine_dir)?;
        let entry_name = destination
            .file_name()
            .with_context(|| format!("cannot name {}", destination.display()))?;
        let live = lstat_at(&quarantine_handle, entry_name).with_context(|| {
            format!(
                "quarantined entry changed under us {}; refusing to guess",
                destination.display()
            )
        })?;
        // Same type, device, and inode as the preflight check above: the
        // entry was not swapped between validation and deletion.
        if live.st_mode & libc::S_IFMT != meta.mode() & libc::S_IFMT
            || live.st_dev != meta.dev()
            || live.st_ino != meta.ino()
        {
            anyhow::bail!(
                "quarantined entry identity changed: {}",
                destination.display()
            );
        }
        if meta.is_dir() {
            // O_DIRECTORY: even if the entry were swapped for a FIFO after
            // the lstat above, the open fails instead of blocking.
            let dir_handle = open_at_nofollow(
                &quarantine_handle,
                entry_name,
                libc::O_RDONLY | libc::O_DIRECTORY,
            )?;
            let stat = fstat_of(&dir_handle, "quarantined entry")?;
            if (stat.st_mode & libc::S_IFMT) != libc::S_IFDIR {
                anyhow::bail!("quarantined entry changed shape: {}", destination.display());
            }
            if stat.st_dev != expected.dev as libc::dev_t
                || stat.st_ino != expected.ino as libc::ino_t
            {
                anyhow::bail!(
                    "quarantined entry identity changed: {}",
                    destination.display()
                );
            }
            remove_tree_fd(&dir_handle, expected.dev)?;
            drop(dir_handle);
            unlink_at(&quarantine_handle, entry_name, true)
                .with_context(|| format!("cannot purge {}", destination.display()))?;
        } else {
            unlink_at(&quarantine_handle, entry_name, false)
                .with_context(|| format!("cannot purge {}", destination.display()))?;
        }
        sync_parent(&destination)?;
        journaled.targets[index].quarantined_as = None;
        journaled.targets[index].purged = true;
        journaled.targets[index].pending_op = None;
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
        // Different size: detected even if the filesystem recycles the inode.
        std::fs::write(&file, "v2-much-longer").unwrap();
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

    /// Fault injection: rename ran, journal did not. The next apply must
    /// discover the moved object and record it instead of stranding.
    #[test]
    fn crash_between_quarantine_and_journal_recovers() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "crash me").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let plan = load_plan(&preview.id).unwrap();
        let dest = quarantine_destination(&plan, 0, &plan.targets[0].path).unwrap();
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        // The crash: object moved, intent journaled, outcome journal lost.
        std::fs::rename(&file, &dest).unwrap();
        let mut crashed = plan.clone();
        crashed.targets[0].pending_op = Some(PendingOp::Quarantine);
        journal_write(&crashed).unwrap();

        let recovered = apply_plan(&preview.id).unwrap();
        assert_eq!(recovered.targets[0].quarantined_as.as_ref(), Some(&dest));
        assert!(recovered.targets[0].pending_op.is_none());
        assert!(!file.exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "crash me");
    }

    /// Fault injection: restore rename ran, journal did not.
    #[test]
    fn crash_between_restore_and_journal_recovers() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "bring me back").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let dest = applied.targets[0].quarantined_as.clone().unwrap();
        // The crash: object moved back, intent journaled, outcome lost.
        std::fs::rename(&dest, &file).unwrap();
        let mut crashed = applied.clone();
        crashed.targets[0].pending_op = Some(PendingOp::Restore);
        journal_write(&crashed).unwrap();

        let recovered = restore(&preview.id).unwrap();
        assert!(recovered.targets[0].restored);
        assert!(recovered.targets[0].pending_op.is_none());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bring me back");
    }

    /// Fault injection: purge delete ran, journal did not.
    #[test]
    fn crash_between_purge_and_journal_recovers() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let dest = applied.targets[0].quarantined_as.clone().unwrap();
        // The crash: bytes deleted, intent journaled, outcome lost.
        std::fs::remove_file(&dest).unwrap();
        let mut crashed = applied.clone();
        crashed.targets[0].pending_op = Some(PendingOp::Purge);
        journal_write(&crashed).unwrap();

        let recovered = purge_apply(&preview.id).unwrap();
        assert!(recovered.targets[0].purged);
        assert!(!dest.exists());
    }

    /// A quarantined directory containing a FIFO must purge without
    /// blocking: classification uses lstat, never open.
    #[test]
    fn purge_directory_with_fifo_completes_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        use std::sync::mpsc;
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let dir = root.path().join("pipedir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "x").unwrap();
        let fifo = dir.join("pipe");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: test-only FIFO under a temp dir.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let preview = plan_removal(root.path(), std::slice::from_ref(&dir)).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        assert!(applied.targets[0].quarantined_as.is_some());

        let id = preview.id.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(purge_apply(&id));
        });
        let purged = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("purge blocked, likely on a FIFO open")
            .unwrap();
        assert!(purged.targets[0].purged);
    }

    /// An unsearchable quarantine fails the live purge path closed: no
    /// bytes removed, plan not marked purged, retry works after access is
    /// restored.
    #[test]
    fn purge_unreadable_quarantine_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: test needs a non-root user for permission checks");
            return;
        }
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let dest = applied.targets[0].quarantined_as.clone().unwrap();
        std::fs::set_permissions(
            &applied.quarantine_dir,
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let err = purge_apply(&preview.id).unwrap_err();
        std::fs::set_permissions(
            &applied.quarantine_dir,
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(dest.exists(), "uninspectable bytes must survive: {err:?}");
        let kept = load_plan(&preview.id).unwrap();
        assert!(!kept.targets[0].purged);
        let purged = purge_apply(&preview.id).unwrap();
        assert!(purged.targets[0].purged);
    }

    /// Interrupted-purge recovery syncs the quarantine, not the original
    /// parent: the source directory may be gone, and recovery must not
    /// demand it back.
    #[test]
    fn purge_recovery_does_not_require_original_parent() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let sub = root.path().join("gone");
        std::fs::create_dir(&sub).unwrap();
        let file = sub.join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let dest = applied.targets[0].quarantined_as.clone().unwrap();
        // The crash: bytes deleted, intent journaled, outcome lost — and the
        // now-empty original parent removed by unrelated cleanup.
        std::fs::remove_file(&dest).unwrap();
        std::fs::remove_dir(&sub).unwrap();
        let mut crashed = applied.clone();
        crashed.targets[0].pending_op = Some(PendingOp::Purge);
        journal_write(&crashed).unwrap();

        let recovered = purge_apply(&preview.id).unwrap();
        assert!(recovered.targets[0].purged);
    }

    /// A second command racing the same plan is refused, not interleaved.
    #[test]
    fn concurrent_plan_use_is_refused() {
        use std::os::unix::io::AsRawFd;
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(plan_lock_path(&preview.id).unwrap())
            .unwrap();
        // SAFETY: test-only contender on a temp lock file.
        let ret = unsafe { libc::flock(_held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(ret, 0);
        let err = apply_plan(&preview.id).unwrap_err();
        assert!(
            err.to_string().contains("locked by another process"),
            "{err}"
        );
        assert!(file.exists(), "nothing may move under contention");
    }

    /// A symlinked quarantine directory would redirect moves off-root.
    #[test]
    fn quarantine_symlink_is_refused() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(QUARANTINE_DIR_NAME)).unwrap();
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let err = apply_plan(&preview.id).unwrap_err();
        assert!(err.to_string().contains("not a real directory"), "{err}");
        assert!(file.exists(), "source must be untouched");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");
    }

    /// A swapped-in symlinked ancestor redirects the restore path off-root.
    #[test]
    fn restore_through_symlink_ancestor_is_refused() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let sub = root.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let file = sub.join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        apply_plan(&preview.id).unwrap();
        // Swap the ancestor for a symlink to an identical layout elsewhere.
        let shadow = tempfile::tempdir().unwrap();
        let shadow_sub = shadow.path().join("sub");
        std::fs::create_dir(&shadow_sub).unwrap();
        std::fs::write(shadow_sub.join("note.txt"), "decoy").unwrap();
        std::fs::remove_dir(&sub).unwrap();
        std::os::unix::fs::symlink(&shadow_sub, &sub).unwrap();
        let err = restore(&preview.id).unwrap_err();
        assert!(err.to_string().contains("symlinked ancestor"), "{err}");
    }

    /// The contained rename walks from the verified root: swapping an
    /// ancestor for a symlink fails the walk instead of moving anything.
    #[test]
    fn contained_rename_through_swapped_ancestor_is_refused() {
        let root = temp_root();
        let sub = root.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let file = sub.join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let dest = root.path().join("out.txt");
        // Swap the ancestor after the paths are recorded: identical layout,
        // hostile link. The dirfd walk must fail, not traverse the link.
        let shadow = tempfile::tempdir().unwrap();
        let shadow_sub = shadow.path().join("sub");
        std::fs::create_dir(&shadow_sub).unwrap();
        std::fs::write(shadow_sub.join("note.txt"), "x").unwrap();
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir(&sub).unwrap();
        std::os::unix::fs::symlink(&shadow_sub, &sub).unwrap();
        let err = rename_contained(root.path(), &file, &dest).unwrap_err();
        assert!(
            err.to_string().contains("unreachable ancestor")
                || err.to_string().contains("symlinked ancestor"),
            "{err}"
        );
        assert!(
            !dest.exists(),
            "nothing may move through the swapped ancestor"
        );
        // Intact tree still renames: move the link aside, restore the dir.
        std::fs::remove_file(&sub).unwrap();
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(&file, "x").unwrap();
        rename_contained(root.path(), &file, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "x");
    }

    /// Quarantine swapped for a symlink after apply: restore and purge both
    /// refuse instead of trusting the recorded destination.
    #[test]
    fn swapped_quarantine_after_apply_is_refused() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let dest = applied.targets[0].quarantined_as.clone().unwrap();
        let shadow = tempfile::tempdir().unwrap();
        std::fs::rename(&dest, shadow.path().join("note.txt")).unwrap();
        crate::exec::remove_dir_all(applied.quarantine_dir.to_str().unwrap()).unwrap();
        std::os::unix::fs::symlink(shadow.path(), &applied.quarantine_dir).unwrap();
        let err = restore(&preview.id).unwrap_err();
        assert!(format!("{err:?}").contains("not a real dir"), "{err:?}");
        let err = purge_apply(&preview.id).unwrap_err();
        assert!(format!("{err:?}").contains("not a real dir"), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(shadow.path().join("note.txt")).unwrap(),
            "x",
            "quarantined bytes must survive the refusal"
        );
    }

    /// Deletion never traverses symlinks: a link planted inside the
    /// quarantined tree removes only itself, never its target.
    #[test]
    fn purge_does_not_traverse_planted_symlinks() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let sub = root.path().join("proj");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "a").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&sub)).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let dest = applied.targets[0].quarantined_as.clone().unwrap();
        // Plant a link to outside bytes after the move.
        let outside = root.path().join("precious.txt");
        std::fs::write(&outside, "do not delete").unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("evil")).unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("evildir")).unwrap();
        let purged = purge_apply(&preview.id).unwrap();
        assert!(purged.targets[0].purged);
        assert!(!dest.exists());
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "do not delete",
            "purge must not traverse planted links"
        );
    }

    /// Deletion-time tree revalidation: protection planted in the quarantined
    /// copy after apply aborts the purge with nothing deleted.
    #[test]
    fn purge_revalidates_tree_at_deletion_time() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let sub = root.path().join("proj");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "a").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&sub)).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let dest = applied.targets[0].quarantined_as.clone().unwrap();
        std::fs::write(dest.join(crate::guard::PROTECT_MARKER), "hold\n").unwrap();
        let err = purge_apply(&preview.id).unwrap_err();
        assert!(err.to_string().contains("protected"), "{err}");
        assert!(dest.join("a.txt").exists(), "nothing may be deleted");
        assert!(!applied.targets[0].purged);
    }

    /// A FIFO swapped in as the quarantine marker must be refused without
    /// opening it: the old blocking open would hang under the plan lock.
    #[test]
    fn fifo_quarantine_marker_is_refused_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        use std::sync::mpsc;
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let first = root.path().join("a.txt");
        std::fs::write(&first, "a").unwrap();
        let preview_a = plan_removal(root.path(), std::slice::from_ref(&first)).unwrap();
        let applied = apply_plan(&preview_a.id).unwrap();
        // Swap the real marker for a FIFO, then run a second plan whose
        // apply must revalidate the quarantine.
        std::fs::remove_file(applied.quarantine_dir.join(crate::guard::PROTECT_MARKER)).unwrap();
        let fifo = applied.quarantine_dir.join(crate::guard::PROTECT_MARKER);
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: test-only FIFO under a temp dir.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let second = root.path().join("b.txt");
        std::fs::write(&second, "b").unwrap();
        let preview_b = plan_removal(root.path(), std::slice::from_ref(&second)).unwrap();
        let id = preview_b.id.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(apply_plan(&id));
        });
        let err = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("marker validation blocked, likely on a FIFO open")
            .unwrap_err();
        assert!(err.to_string().contains("unexpected marker"), "{err}");
        assert!(!first.exists(), "first target stays quarantined");
        assert!(second.exists(), "second target must not move on refusal");
    }

    /// Traversal-time protection: remove_tree_fd itself refuses marker
    /// names, so protection appearing after preflight still aborts.
    #[test]
    fn remove_tree_fd_refuses_marker_names() {
        use std::os::unix::fs::MetadataExt;
        let root = temp_root();
        let dir = root.path().join("tree");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join(crate::guard::PROTECT_MARKER), "hold\n").unwrap();
        let dev = std::fs::metadata(&dir).unwrap().dev();
        let handle = open_dir_nofollow(&dir).unwrap();
        let err = remove_tree_fd(&handle, dev).unwrap_err();
        assert!(err.to_string().contains("protected"), "{err}");
        assert!(dir.join("a.txt").exists(), "nothing may be deleted");
    }

    /// Partial batch: A quarantined, B never applied. Restore moves A and
    /// leaves B untouched instead of rejecting the batch.
    #[test]
    fn restore_partial_batch_restores_subset() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let fa = root.path().join("a.txt");
        let fb = root.path().join("b.txt");
        std::fs::write(&fa, "aaa").unwrap();
        std::fs::write(&fb, "bbb").unwrap();
        let preview = plan_removal(root.path(), &[fa.clone(), fb.clone()]).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        // Rewind B to never-applied by hand (operator-grade surgery on a
        // temp journal): move it back, clear its record.
        let db = applied.targets[1].quarantined_as.clone().unwrap();
        std::fs::rename(&db, &fb).unwrap();
        let mut rewound = applied.clone();
        rewound.targets[1].quarantined_as = None;
        rewound.targets[1].restored = false;
        rewound.targets[1].pending_op = None;
        journal_write(&rewound).unwrap();

        let restored = restore(&preview.id).unwrap();
        assert!(restored.targets[0].restored);
        assert!(!restored.targets[1].restored);
        assert_eq!(std::fs::read_to_string(&fa).unwrap(), "aaa");
        assert_eq!(std::fs::read_to_string(&fb).unwrap(), "bbb");
    }

    /// Intent journaled, mutation never ran: the op clears intent and
    /// proceeds instead of stranding.
    #[test]
    fn intent_without_mutation_proceeds() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let mut plan = load_plan(&preview.id).unwrap();
        plan.targets[0].pending_op = Some(PendingOp::Quarantine);
        journal_write(&plan).unwrap();

        let applied = apply_plan(&preview.id).unwrap();
        assert!(applied.targets[0].quarantined_as.is_some());
        assert!(applied.targets[0].pending_op.is_none());
        assert!(!file.exists());
    }

    /// Unreadable quarantine during purge reconcile: intent preserved,
    /// nothing marked purged.
    #[test]
    fn purge_reconcile_preserves_intent_on_unreadable_quarantine() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: test needs a non-root user for permission checks");
            return;
        }
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        let applied = apply_plan(&preview.id).unwrap();
        let mut crashed = applied.clone();
        crashed.targets[0].pending_op = Some(PendingOp::Purge);
        journal_write(&crashed).unwrap();
        let qdir = crashed.quarantine_dir.clone();
        // Revoke access: entry stat now fails with EACCES, not NotFound.
        std::fs::set_permissions(&qdir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = purge_apply(&preview.id).unwrap_err();
        std::fs::set_permissions(&qdir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(format!("{err:?}").contains("intent preserved"), "{err:?}");
        let kept = load_plan(&preview.id).unwrap();
        assert_eq!(kept.targets[0].pending_op, Some(PendingOp::Purge));
        assert!(!kept.targets[0].purged);
        // Access restored: the purge completes normally.
        let purged = purge_apply(&preview.id).unwrap();
        assert!(purged.targets[0].purged);
    }

    /// The root itself must be a real directory, not a symlink.
    #[test]
    fn ancestry_rejects_symlinked_root() {
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let link = root.path().join("rootlink");
        std::os::unix::fs::symlink(root.path(), &link).unwrap();
        let file = root.path().join("note.txt");
        let err = assert_ancestry(&file, &link).unwrap_err();
        assert!(err.to_string().contains("not a real directory"), "{err}");
    }

    /// State and quarantine directories are private to the owner.
    #[test]
    fn state_dirs_are_owner_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root();
        let _state = with_state_dir(root.path());
        let file = root.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let preview = plan_removal(root.path(), std::slice::from_ref(&file)).unwrap();
        apply_plan(&preview.id).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&plans_dir()), 0o700);
        let plan = load_plan(&preview.id).unwrap();
        assert_eq!(mode(&plan.quarantine_dir), 0o700);
    }
}
