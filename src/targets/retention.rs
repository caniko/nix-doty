//! Retention for persistent, explicitly configured state outside scratch.

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use std::os::unix::fs::OpenOptionsExt;

#[path = "model_inventory.rs"]
mod model_inventory;
pub use model_inventory::MANAGED_MODELS;
#[path = "persistent_storage.rs"]
mod persistent_storage;
pub use persistent_storage::{NIX_BUILDS, SERVICE_STORAGE};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Incremental,
    Cargo,
    Packages,
    Models,
    Retirement,
}

struct RetentionFramework {
    name: &'static str,
    summary: &'static str,
    variants: &'static [&'static dyn Variant],
}

impl Framework for RetentionFramework {
    fn name(&self) -> &'static str {
        self.name
    }
    fn summary(&self) -> &'static str {
        self.summary
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        self.variants
    }
}

struct RetentionVariant {
    framework: &'static RetentionFramework,
    name: &'static str,
    kind: Kind,
    report_only: bool,
}

static CARGO_FRAMEWORK: RetentionFramework = RetentionFramework {
    name: "cargo-builds",
    summary: "Persistent Cargo outputs: descendant-age retention and active-build protection",
    variants: &[&CARGO_REPORT, &INCREMENTAL_PRUNE, &CARGO_PRUNE],
};
static PACKAGE_FRAMEWORK: RetentionFramework = RetentionFramework {
    name: "package-caches",
    summary: "Explicit package/compiler cache paths with bounded stale-entry retention",
    variants: &[&PACKAGE_REPORT, &PACKAGE_PRUNE],
};
static MODEL_FRAMEWORK: RetentionFramework = RetentionFramework {
    name: "model-directories",
    summary: "Explicitly retired model directories, preserving configured pins and active mappings",
    variants: &[&MODEL_REPORT, &MODEL_PRUNE],
};
static RETIREMENT_FRAMEWORK: RetentionFramework = RetentionFramework {
    name: "retained-state",
    summary: "Legacy backups and campaign data: inspect, then quarantine approved paths; purge separately",
    variants: &[&RETIREMENT_REPORT, &RETIREMENT_QUARANTINE],
};

pub static CARGO_BUILDS: &dyn Framework = &CARGO_FRAMEWORK;
pub static PACKAGE_CACHES: &dyn Framework = &PACKAGE_FRAMEWORK;
pub static MODEL_DIRECTORIES: &dyn Framework = &MODEL_FRAMEWORK;
pub static RETAINED_STATE: &dyn Framework = &RETIREMENT_FRAMEWORK;

static CARGO_REPORT: RetentionVariant = RetentionVariant {
    framework: &CARGO_FRAMEWORK,
    name: "disk-report",
    kind: Kind::Cargo,
    report_only: true,
};
static INCREMENTAL_PRUNE: RetentionVariant = RetentionVariant {
    framework: &CARGO_FRAMEWORK,
    name: "prune-incremental",
    kind: Kind::Incremental,
    report_only: false,
};
static CARGO_PRUNE: RetentionVariant = RetentionVariant {
    framework: &CARGO_FRAMEWORK,
    name: "prune-targets",
    kind: Kind::Cargo,
    report_only: false,
};
static PACKAGE_REPORT: RetentionVariant = RetentionVariant {
    framework: &PACKAGE_FRAMEWORK,
    name: "disk-report",
    kind: Kind::Packages,
    report_only: true,
};
static PACKAGE_PRUNE: RetentionVariant = RetentionVariant {
    framework: &PACKAGE_FRAMEWORK,
    name: "prune-stale",
    kind: Kind::Packages,
    report_only: false,
};
static MODEL_REPORT: RetentionVariant = RetentionVariant {
    framework: &MODEL_FRAMEWORK,
    name: "disk-report",
    kind: Kind::Models,
    report_only: true,
};
static MODEL_PRUNE: RetentionVariant = RetentionVariant {
    framework: &MODEL_FRAMEWORK,
    name: "prune-retired",
    kind: Kind::Models,
    report_only: false,
};
static RETIREMENT_REPORT: RetentionVariant = RetentionVariant {
    framework: &RETIREMENT_FRAMEWORK,
    name: "disk-report",
    kind: Kind::Retirement,
    report_only: true,
};
static RETIREMENT_QUARANTINE: RetentionVariant = RetentionVariant {
    framework: &RETIREMENT_FRAMEWORK,
    name: "quarantine-approved",
    kind: Kind::Retirement,
    report_only: false,
};

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct Settings {
    roots: Vec<PathBuf>,
    paths: Vec<PathBuf>,
    pinned_paths: Vec<PathBuf>,
    retired_paths: Vec<PathBuf>,
    approved_paths: Vec<PathBuf>,
    min_age_days: u32,
    max_entries: u64,
    max_depth: u32,
    entry_depth: u32,
    #[serde(skip)]
    check_processes: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            roots: vec![],
            paths: vec![],
            pinned_paths: vec![],
            retired_paths: vec![],
            approved_paths: vec![],
            min_age_days: 30,
            max_entries: 1_000_000,
            max_depth: 64,
            entry_depth: 1,
            check_processes: true,
        }
    }
}

impl Settings {
    fn parse(value: &Value) -> Result<Self> {
        Self::parse_with_roots(value, true)
    }

    // Reports must preserve inaccessible/missing roots as inspection issues.
    fn parse_with_roots(value: &Value, require_existing: bool) -> Result<Self> {
        let result: Self = if value.is_null() {
            Self::default()
        } else {
            serde_json::from_value(value.clone())?
        };
        ensure!(result.min_age_days > 0, "minAgeDays must be positive");
        ensure!(
            result.max_entries > 0 && result.max_entries <= 10_000_000,
            "maxEntries must be 1..=10000000"
        );
        ensure!(
            result.max_depth > 0 && result.max_depth <= 128,
            "maxDepth must be 1..=128"
        );
        ensure!(
            result.entry_depth > 0 && result.entry_depth <= 4,
            "entryDepth must be 1..=4"
        );
        for path in result
            .roots
            .iter()
            .chain(&result.paths)
            .chain(&result.pinned_paths)
            .chain(&result.retired_paths)
            .chain(&result.approved_paths)
        {
            ensure!(
                path.is_absolute()
                    && !path
                        .components()
                        .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
                "retention paths must be absolute without dot components: {}",
                path.display()
            );
            ensure!(
                !path.to_string_lossy().contains(','),
                "retention paths cannot contain commas"
            );
        }
        for root in &result.roots {
            ensure!(
                root.parent().is_some(),
                "filesystem root is not a retention root"
            );
            if require_existing {
                real_directory(root)?;
            }
        }
        for (index, root) in result.roots.iter().enumerate() {
            ensure!(
                !result
                    .roots
                    .iter()
                    .skip(index + 1)
                    .any(|other| other.starts_with(root) || root.starts_with(other)),
                "retention roots overlap"
            );
        }
        for path in result
            .paths
            .iter()
            .chain(&result.retired_paths)
            .chain(&result.approved_paths)
        {
            result.root_for(path)?;
        }
        for (index, path) in result.paths.iter().enumerate() {
            ensure!(
                !result
                    .paths
                    .iter()
                    .skip(index + 1)
                    .any(|other| path.starts_with(other) || other.starts_with(path)),
                "configured paths overlap"
            );
            ensure!(
                !path
                    .strip_prefix(result.root_for(path)?)?
                    .components()
                    .any(|part| excluded(part.as_os_str())),
                "configured path contains an excluded component: {}",
                path.display()
            );
        }
        Ok(result)
    }

    fn root_for(&self, path: &Path) -> Result<&Path> {
        self.roots
            .iter()
            .find(|root| path != root.as_path() && path.starts_with(root))
            .map(PathBuf::as_path)
            .with_context(|| {
                format!(
                    "path is not strictly below a configured root: {}",
                    path.display()
                )
            })
    }
}

fn real_directory(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        ensure!(
            fs::symlink_metadata(&current)?.is_dir(),
            "not a real directory: {}",
            current.display()
        );
    }
    Ok(())
}

fn guard_retention_path(path: &Path, root: &Path) -> Result<()> {
    real_directory(root)?;
    crate::guard::guard_path(path, root)?;
    let start = if fs::symlink_metadata(path)?.is_dir() {
        path
    } else {
        path.parent().context("candidate has no parent")?
    };
    for ancestor in start
        .ancestors()
        .take_while(|ancestor| ancestor.starts_with(root))
    {
        ensure!(
            fs::symlink_metadata(ancestor.join(crate::guard::PROTECT_MARKER))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "protected or unobservable ancestor: {}",
            ancestor.display()
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Scan {
    logical_bytes: u64,
    allocated_bytes: u64,
    entries: u64,
    newest: Option<SystemTime>,
    stamp: u64,
    root_ctime: (i64, i64),
    complete: bool,
}

impl Scan {
    fn matches_after_rename(&self, original: &Self) -> bool {
        // Quarantine's rename changes only the candidate root's ctime.
        // Pre-rename checks compare it as well, including single-file candidates.
        Self {
            root_ctime: original.root_ctime,
            ..self.clone()
        } == *original
    }
}

struct Candidate {
    path: PathBuf,
    root: PathBuf,
    activity_root: PathBuf,
    scan: Scan,
}

struct Selection {
    candidates: Vec<Candidate>,
    logical_bytes: u64,
    allocated_bytes: u64,
    complete: bool,
    skipped: Vec<String>,
}

fn excluded(name: &std::ffi::OsStr) -> bool {
    ["tmp", ".tmp", ".git", ".doty-protect", ".doty-quarantine"]
        .iter()
        .any(|excluded| name == *excluded)
}

/// A single report-wide budget. No symlink is followed, incomplete or protected
/// trees cannot become deletion candidates, and age includes all descendants.
fn scan_tree(path: &Path, settings: &Settings, remaining: &mut u64) -> Result<Scan> {
    let device = fs::symlink_metadata(path)?.dev();
    let mut scan = Scan {
        complete: true,
        ..Scan::default()
    };
    let mut stack = vec![(path.to_path_buf(), 0)];
    let mut inodes = BTreeSet::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    while let Some((current, depth)) = stack.pop() {
        if *remaining == 0 {
            scan.complete = false;
            break;
        }
        *remaining -= 1;
        scan.entries += 1;
        let meta = match fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(_) => {
                scan.complete = false;
                continue;
            }
        };
        if current.file_name().is_some_and(excluded)
            || meta.dev() != device
            || (!meta.is_dir() && !meta.is_file())
        {
            scan.complete = false;
            continue;
        }
        current.strip_prefix(path)?.hash(&mut hasher);
        (
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
        )
            .hash(&mut hasher);
        if depth == 0 {
            scan.root_ctime = (meta.ctime(), meta.ctime_nsec());
        } else {
            // Descendant ctimes remain stable across the quarantine rename.
            (meta.ctime(), meta.ctime_nsec()).hash(&mut hasher);
        }
        match meta.modified() {
            Ok(time) => scan.newest = Some(scan.newest.map_or(time, |previous| previous.max(time))),
            Err(_) => scan.complete = false,
        }
        if meta.is_file() {
            scan.logical_bytes = scan.logical_bytes.saturating_add(meta.len());
            if inodes.insert((meta.dev(), meta.ino())) {
                scan.allocated_bytes = scan
                    .allocated_bytes
                    .saturating_add(meta.blocks().saturating_mul(512));
            }
        } else if depth >= settings.max_depth {
            scan.complete = false;
        } else {
            match children(&current, remaining) {
                Ok(entries) => {
                    stack.extend(entries.into_iter().rev().map(|entry| (entry, depth + 1)))
                }
                Err(_) => scan.complete = false,
            }
        }
    }
    scan.stamp = hasher.finish();
    Ok(scan)
}

fn children(path: &Path, remaining: &mut u64) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        ensure!(*remaining > 0, "entry budget exhausted");
        *remaining -= 1;
        entries.push(entry?.path());
    }
    entries.sort();
    Ok(entries)
}

fn at_depth(path: &Path, depth: u32, remaining: &mut u64) -> Result<Vec<PathBuf>> {
    let mut entries = vec![path.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for entry in entries {
            let meta = fs::symlink_metadata(&entry)?;
            if meta.is_dir() && !entry.file_name().is_some_and(excluded) {
                next.extend(children(&entry, remaining)?);
            }
        }
        entries = next;
    }
    Ok(entries)
}

fn cargo_proof(path: &Path) -> bool {
    let tag = path.join("CACHEDIR.TAG");
    let tagged = fs::symlink_metadata(&tag).is_ok_and(|meta| meta.is_file() && meta.len() <= 4096)
        && fs::read(tag)
            .is_ok_and(|bytes| bytes.starts_with(b"Signature: 8a477f597d28d172789f06886806bc55"));
    tagged
        || ["debug", "release"].iter().any(|profile| {
            fs::symlink_metadata(path.join(profile)).is_ok_and(|meta| meta.is_dir())
                && fs::symlink_metadata(path.join(profile).join(".fingerprint"))
                    .is_ok_and(|meta| meta.is_dir())
        })
}

fn incremental_entries(path: &Path, remaining: &mut u64) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for first in at_depth(path, 1, remaining)? {
        if !fs::symlink_metadata(&first)?.is_dir() || first.file_name().is_some_and(excluded) {
            continue;
        }
        let direct = first.join("incremental");
        if fs::symlink_metadata(&direct).is_ok_and(|meta| meta.is_dir()) {
            entries.extend(at_depth(&direct, 1, remaining)?);
            continue;
        }
        if fs::symlink_metadata(first.join(".fingerprint")).is_ok() {
            continue;
        }
        // Cross-target profiles, e.g. target/wasm32-unknown-unknown/debug.
        for second in at_depth(&first, 1, remaining)? {
            let nested = second.join("incremental");
            if fs::symlink_metadata(&second).is_ok_and(|meta| meta.is_dir())
                && fs::symlink_metadata(&nested).is_ok_and(|meta| meta.is_dir())
            {
                entries.extend(at_depth(&nested, 1, remaining)?);
            }
        }
    }
    Ok(entries)
}

fn select(kind: Kind, settings: &Settings) -> Result<Selection> {
    let mut selection = Selection {
        candidates: vec![],
        logical_bytes: 0,
        allocated_bytes: 0,
        complete: true,
        skipped: vec![],
    };
    let mut remaining = settings.max_entries;
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(
            u64::from(settings.min_age_days) * 86400,
        ))
        .context("invalid age cutoff")?;
    let mut owners = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for path in &settings.paths {
        let root = settings.root_for(path)?;
        if fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            selection
                .skipped
                .push(format!("{}: missing", path.display()));
            continue;
        }
        if fs::symlink_metadata(path.join(".git")).is_ok()
            || fs::symlink_metadata(path.join(crate::guard::PROTECT_MARKER)).is_ok()
        {
            selection
                .skipped
                .push(format!("{}: protected repository/state", path.display()));
            continue;
        }
        guard_retention_path(path, root)?;
        if matches!(kind, Kind::Cargo | Kind::Incremental) && !cargo_proof(path) {
            selection
                .skipped
                .push(format!("{}: no Cargo output proof", path.display()));
            continue;
        }
        let entries = match kind {
            Kind::Incremental => incremental_entries(path, &mut remaining),
            Kind::Packages => at_depth(path, settings.entry_depth, &mut remaining),
            _ => Ok(vec![path.clone()]),
        };
        let entries = match entries {
            Ok(entries) => entries,
            Err(error) => {
                selection.complete = false;
                selection
                    .skipped
                    .push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        for entry in entries {
            if !seen.insert(entry.clone()) {
                continue;
            }
            if entry.file_name().is_some_and(excluded) {
                selection
                    .skipped
                    .push(format!("{}: excluded", entry.display()));
                continue;
            }
            let scan = scan_tree(&entry, settings, &mut remaining)?;
            selection.logical_bytes = selection.logical_bytes.saturating_add(scan.logical_bytes);
            selection.allocated_bytes = selection
                .allocated_bytes
                .saturating_add(scan.allocated_bytes);
            selection.complete &= scan.complete;
            let reason = if !scan.complete {
                Some("incomplete/protected/excluded tree")
            } else if settings
                .pinned_paths
                .iter()
                .any(|pin| entry.starts_with(pin) || pin.starts_with(&entry))
            {
                Some("pinned")
            } else if kind == Kind::Models && !settings.retired_paths.contains(&entry) {
                Some("not explicitly retired")
            } else if kind == Kind::Retirement && !settings.approved_paths.contains(&entry) {
                Some("not approved for retirement")
            } else if scan.newest.is_none_or(|time| time > cutoff) {
                Some("recent descendant activity")
            } else {
                None
            };
            if let Some(reason) = reason {
                selection
                    .skipped
                    .push(format!("{}: {reason}", entry.display()));
                continue;
            }
            // The process-use surface is the entire Cargo target, even when
            // only incremental children are selected (Cargo holds its lock there).
            let activity_root = if matches!(kind, Kind::Incremental | Kind::Packages) {
                path.clone()
            } else {
                entry.clone()
            };
            if settings.check_processes {
                let uid = fs::symlink_metadata(&entry)?.uid();
                if let std::collections::btree_map::Entry::Vacant(slot) = owners.entry(uid) {
                    let process_use = process_paths(uid)?;
                    if process_use.unknown {
                        selection.skipped.push(format!(
                            "process evidence for UID {uid}: {}",
                            serde_json::to_string(&process_use.issues)?
                        ));
                    }
                    slot.insert(process_use);
                }
                if owners[&uid].references(&activity_root) {
                    selection
                        .skipped
                        .push(format!("{}: active process reference", entry.display()));
                    continue;
                }
                if owners[&uid].unknown {
                    selection.skipped.push(format!(
                        "{}: process use cannot be verified (permissions/budget)",
                        entry.display()
                    ));
                    continue;
                }
            }
            if let Err(error) = guard_retention_path(&entry, root) {
                selection
                    .skipped
                    .push(format!("{}: {error}", entry.display()));
                continue;
            }
            selection.candidates.push(Candidate {
                path: entry,
                root: root.to_path_buf(),
                activity_root,
                scan,
            });
        }
    }
    selection.candidates.sort_by(|a, b| a.path.cmp(&b.path));
    for pair in selection.candidates.windows(2) {
        ensure!(
            !pair[1].path.starts_with(&pair[0].path),
            "overlapping retention candidates"
        );
    }
    Ok(selection)
}

/// Inspect references for the data owner's processes. Unobservable live
/// processes fail closed. Exiting processes are harmless; no environment or
/// credential files are read. Maps catch model weights with closed file handles.
#[derive(Default)]
struct ProcessUse {
    paths: BTreeSet<PathBuf>,
    unknown: bool,
    issues: Vec<String>,
}

impl ProcessUse {
    fn references(&self, path: &Path) -> bool {
        self.paths
            .range(path.to_path_buf()..)
            .next()
            .is_some_and(|open| open.starts_with(path))
    }
}

fn process_paths(uid: u32) -> Result<ProcessUse> {
    let mut result = ProcessUse::default();
    let mut remaining = 1_000_000u64;
    let root_operator = fs::metadata("/proc/self")?.uid() == 0;
    for process in fs::read_dir("/proc")? {
        let process = process?;
        if process
            .file_name()
            .to_str()
            .is_none_or(|name| name.parse::<u32>().is_err())
        {
            continue;
        }
        let path = process.path();
        let metadata = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !root_operator && metadata.uid() != uid {
            continue;
        }
        let references = (|| -> Result<()> {
            process_link(&path.join("cwd"), &mut result.paths)?;
            for fd in fs::read_dir(path.join("fd"))? {
                ensure!(remaining > 0, "process reference budget exhausted");
                remaining -= 1;
                process_link(&fd?.path(), &mut result.paths)?;
            }
            use std::io::Read;
            let mut maps = String::new();
            fs::File::open(path.join("maps"))?
                .take(16 * 1024 * 1024 + 1)
                .read_to_string(&mut maps)?;
            ensure!(
                maps.len() <= 16 * 1024 * 1024,
                "process map byte budget exhausted"
            );
            for line in maps.lines() {
                ensure!(remaining > 0, "process reference budget exhausted");
                remaining -= 1;
                if let Some(start) = line.find('/') {
                    result.paths.insert(PathBuf::from(
                        line[start..]
                            .trim_end_matches(" (deleted)")
                            .replace("\\040", " "),
                    ));
                }
            }
            Ok(())
        })();
        if let Err(error) = references {
            // ESRCH means a process has exited (its /proc directory may remain
            // for a zombie), so it cannot retain cwd/fds/mappings.
            let exited = error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH)
            });
            if !exited && path.exists() {
                result.unknown = true;
                if result.issues.len() < 8 {
                    result.issues.push(format!("{}: {error}", path.display()));
                }
            }
        }
    }
    Ok(result)
}

fn process_link(path: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    match fs::read_link(path) {
        Ok(link) => {
            paths.insert(link);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot read process reference {}", path.display()));
        }
    }
    Ok(())
}

fn recheck(candidate: &Candidate, settings: &Settings) -> Result<()> {
    guard_retention_path(&candidate.path, &candidate.root)?;
    let mut remaining = settings.max_entries;
    let scan = scan_tree(&candidate.path, settings, &mut remaining)?;
    ensure!(
        scan.complete && scan == candidate.scan,
        "candidate changed since inspection: {}",
        candidate.path.display()
    );
    if settings.check_processes {
        let uid = fs::symlink_metadata(&candidate.path)?.uid();
        let process_use = process_paths(uid)?;
        ensure!(
            !process_use.unknown && !process_use.references(&candidate.activity_root),
            "candidate use is active or unverifiable: {}",
            candidate.path.display()
        );
    }
    Ok(())
}

impl Variant for RetentionVariant {
    fn name(&self) -> &'static str {
        self.name
    }
    fn framework(&self) -> &'static dyn Framework {
        self.framework
    }
    fn tier(&self) -> Tier {
        if self.report_only {
            Tier::ReportOnly
        } else {
            Tier::Confirm
        }
    }
    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&Value::Null)
    }
    fn apply(&self, apply: bool, force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(apply, force, &Value::Null)
    }

    fn inspect_with_settings(&self, value: &Value) -> Result<Inspection> {
        let settings = Settings::parse(value)?;
        let selection = select(self.kind, &settings)?;
        let eligible: u64 = selection
            .candidates
            .iter()
            .map(|candidate| candidate.scan.logical_bytes)
            .sum();
        let quarantine = self.kind == Kind::Retirement && !self.report_only;
        Ok(Inspection {
            framework: self.framework.name,
            variant: self.name,
            path: settings
                .paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            // Reflinks, sparse files and compression defeat a footprint-based
            // reclaim estimate. Reclaim must not sum logical candidate bytes.
            size_bytes: if self.report_only {
                Some(selection.logical_bytes)
            } else if quarantine {
                Some(0)
            } else {
                None
            },
            age_oldest_days: None,
            would_remove: if self.report_only {
                0
            } else {
                selection.candidates.len() as u64
            },
            notes: format!(
                "footprint: {} logical bytes, {} allocated bytes (hardlinks deduplicated within each tree; reflinks/compression not accounted); eligible: {eligible} logical bytes in {} paths; complete: {}; quarantine frees no space until separate purge; candidates: {}; skipped: {}",
                selection.logical_bytes,
                selection.allocated_bytes,
                selection.candidates.len(),
                selection.complete,
                serde_json::to_string(
                    &selection
                        .candidates
                        .iter()
                        .map(|candidate| &candidate.path)
                        .collect::<Vec<_>>()
                )?,
                serde_json::to_string(&selection.skipped)?
            ),
        })
    }

    fn apply_with_settings(&self, apply: bool, force: bool, value: &Value) -> Result<ApplyReport> {
        self.apply_settings(apply, force, &Settings::parse(value)?)
    }
}

impl RetentionVariant {
    fn apply_settings(&self, apply: bool, force: bool, settings: &Settings) -> Result<ApplyReport> {
        let selection = select(self.kind, settings)?;
        let mut report = ApplyReport {
            framework: self.framework.name,
            variant: self.name,
            removed: 0,
            freed_bytes: 0,
            skipped: selection.skipped.len() as u64,
            errors: vec![],
        };
        if !apply || self.report_only || !force {
            report.skipped += selection.candidates.len() as u64;
            if !self.report_only {
                report.errors.push(format!(
                    "dry-run: {} candidates; mutations require --apply --force",
                    selection.candidates.len()
                ));
            }
            return Ok(report);
        }
        // Completeness is per candidate: a protected or budget-limited sibling
        // cannot authorize its own removal or block an independently verified tree.
        let mut groups: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        for candidate in &selection.candidates {
            recheck(candidate, settings)?;
            groups
                .entry(candidate.root.clone())
                .or_default()
                .push(candidate.path.clone());
        }
        let mut plans = Vec::new();
        let mut before = BTreeMap::new();
        for (root, paths) in &groups {
            let usage = crate::mount::df(&root.to_string_lossy())?;
            before
                .entry(usage.device)
                .or_insert((root.clone(), usage.available_bytes));
            plans.push(crate::rm::plan_removal(root, paths)?);
        }
        for candidate in &selection.candidates {
            recheck(candidate, settings)?;
        }
        for plan in plans {
            eprintln!(
                "Retention plan {}: restore with `doty restore --plan {}`; quarantine alone frees no disk space",
                plan.id, plan.id
            );
            let applied = crate::rm::apply_plan(&plan.id)?;
            if self.kind != Kind::Retirement {
                // Recheck descendants after rename, before irreversible purge.
                // On failure the journal retains a restorable quarantine.
                for target in &applied.targets {
                    let candidate = selection
                        .candidates
                        .iter()
                        .find(|candidate| candidate.path == target.path)
                        .context("unknown retention target")?;
                    let destination = target
                        .quarantined_as
                        .as_ref()
                        .context("target was not quarantined")?;
                    let mut remaining = settings.max_entries;
                    let scan = scan_tree(destination, settings, &mut remaining)?;
                    ensure!(
                        scan.complete && scan.matches_after_rename(&candidate.scan),
                        "candidate changed during quarantine; retained in plan {}",
                        plan.id
                    );
                    if settings.check_processes {
                        let uid = fs::symlink_metadata(destination)?.uid();
                        let process_use = process_paths(uid)?;
                        ensure!(
                            !process_use.unknown && !process_use.references(destination),
                            "quarantined candidate use active or unverifiable; retained in plan {}",
                            plan.id
                        );
                    }
                }
                crate::rm::purge_apply(&plan.id)?;
            } else {
                eprintln!(
                    "Review this retirement, then reclaim with `doty purge --plan {} --apply`",
                    plan.id
                );
            }
            report.removed += applied.targets.len() as u64;
        }
        if self.kind != Kind::Retirement {
            for (root, available) in before.into_values() {
                report.freed_bytes = report.freed_bytes.saturating_add(
                    crate::mount::df(&root.to_string_lossy())?
                        .available_bytes
                        .saturating_sub(available),
                );
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
