//! Metadata-only analysis of agent scratch space.
//!
//! This module never reads file contents and never mutates anything. Every
//! scan walks filesystem metadata (names, types, sizes, mtimes) within a
//! caller-supplied entry budget and depth limit.
//!
//! Schema version 2 JSON fields:
//!
//! - `mode`: `"inventory"` (top-level entries only, no descent) or `"subtree"`
//!   (recursive scan). Inventory directory ages are the directory's own mtime,
//!   NOT descendant activity; inventory sizes cover top-level files only.
//! - `logical_bytes`: sum of regular-file lengths by pathname. Hardlinks count
//!   per pathname and sparse files report logical size: this is NOT allocated
//!   or reclaimable disk space.
//! - `observed_entries`: metadata records classified (including skipped ones).
//! - `discovered_entries`: top-level dirents seen before filtering.
//! - `matching_entries`: entries passing the age filter (before `--limit`).
//! - `complete`: false when any subtree hit an error, the entry budget, or the
//!   depth limit. Skipped symlinks, special files, and other-device entries do
//!   NOT affect `complete`; they set `skipped_reason` and land in
//!   `review_queue` instead.
//! - `oldest_age_days` / `newest_age_days`: descendant-inclusive whole-day ages
//!   over observed entries only. `None` means unknown (nothing observable, a
//!   metadata error, or an intentionally skipped subtree).
//! - `direct_age_days`: age of the top-level path itself.
//! - `review_queue`: entries needing human attention (incomplete, unknown age,
//!   or skipped). Unknown entries are never ranked as confidently old.
//! - `issues`: bounded arbitrary sample of skipped/error notes in discovery
//!   order; `issues_total`, `issues_by_reason`, and `issues_omitted` are the
//!   authoritative counts.
//!
//! Exit behavior: the command exits 0 whenever the root could be opened and
//! enumerated, even when `complete` is false. A non-zero exit means a hard
//! error (unopenable root, unreadable root listing). `complete: false` is data,
//! not failure.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Subcommand)]
pub enum Agent {
    /// Inspect OpenCode scratch metadata; never authorizes deletion
    #[command(long_about = "Metadata-only scan of OpenCode scratch space.\n\
        \n\
        Default root: /data/scratch/tmp/opencode. Use --path to scope a subtree.\n\
        \n\
        MODES: default is a recursive `subtree` scan. --inventory lists every\n\
        top-level sibling with direct metadata only (no descent), so one huge\n\
        directory cannot starve the entry budget before siblings are seen.\n\
        Scan explicit subtrees afterwards with --path, each with its own budget.\n\
        \n\
        SORT: --sort size ranks by logical bytes; --sort oldest ranks by newest\n\
        observed modification (least recently active first). Unknown or\n\
        incomplete entries always sort last and appear in `review_queue`.\n\
        \n\
        SAFETY: symlinks are never followed (including every --path component),\n\
        other devices are not crossed, file contents are never read, and nothing\n\
        is deleted. Logical bytes are not reclaimable space. Modification age\n\
        does not prove inactivity, ownership, or disposability.\n\
        \n\
        EXIT: 0 whenever the root was opened and enumerated, even if the report\n\
        is partial (complete: false). Non-zero only on hard errors such as an\n\
        unopenable root. Linux only: other platforms exit non-zero rather than\n\
        run a weaker scan.")]
    Opencode(Options),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
enum SortMode {
    #[default]
    Size,
    Oldest,
}

impl std::fmt::Display for SortMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SortMode::Size => write!(f, "size"),
            SortMode::Oldest => write!(f, "oldest"),
        }
    }
}

#[derive(Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct Options {
    #[arg(long, default_value = "/data/scratch/tmp/opencode")]
    path: PathBuf,
    #[arg(long)]
    json: bool,
    /// Show only complete entries whose newest observed modification is this old
    #[arg(long)]
    older_than_days: Option<u32>,
    /// Maximum displayed top-level entries (totals cover the entire scan)
    #[arg(long, default_value = "50", value_parser = clap::value_parser!(u32).range(1..))]
    limit: u32,
    /// Global metadata/iteration budget, not a per-directory limit
    #[arg(long, default_value = "100000", value_parser = clap::value_parser!(u32).range(1..))]
    max_entries: u32,
    /// Maximum directory descent below the root (ignored by --inventory)
    #[arg(long, default_value = "64", value_parser = clap::value_parser!(u32).range(1..=128))]
    max_depth: u32,
    /// List top-level entries with direct metadata only; do not descend
    #[arg(long)]
    inventory: bool,
    /// Entry ordering: `size` by logical bytes, `oldest` by least recent activity
    #[arg(long, value_enum, default_value_t = SortMode::Size)]
    sort: SortMode,
    /// Maximum issues retained in output; counts are always complete
    #[arg(long, default_value = "50", value_parser = clap::value_parser!(u32).range(0..=10000))]
    max_issues: u32,
}

impl Agent {
    pub fn run(self) -> Result<()> {
        let Self::Opencode(options) = self;
        let report = scan(&options)?;
        if options.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "{} — {} scan (schema {}), {} order",
                serde_json::to_string(&report.root)?,
                report.mode,
                report.schema_version,
                report.sort
            );
            println!(
                "{} logical bytes across {} observed records ({} top-level discovered); complete: {}",
                report.logical_bytes,
                report.observed_entries,
                report.discovered_entries,
                report.complete
            );
            println!(
                "{} matching entries ({} shown, limit {}); {} complete, {} incomplete, {} skipped",
                report.matching_entries,
                report.entries.len(),
                report.limit,
                report.complete_entries,
                report.incomplete_entries,
                report.skipped_entries
            );
            println!(
                "issues: {} total, {} shown ({} omitted)",
                report.issues_total,
                report.issues.len(),
                report.issues_omitted
            );
            for (reason, count) in &report.issues_by_reason {
                println!("  {count}x {reason}");
            }
            println!(
                "review queue: {} total, {} shown",
                report.review_queue_total,
                report.review_queue.len()
            );
            for item in &report.review_queue {
                println!(
                    "  review: {} ({})",
                    serde_json::to_string(&item.path)?,
                    item.reason
                );
            }
            println!(
                "{:>14} {:>10} {:>10} {:>8} Path (JSON escaped)",
                "Bytes", "NewestDays", "Complete", "Kind"
            );
            for entry in &report.entries {
                println!(
                    "{:>14} {:>10} {:>10} {:>8} {}",
                    entry.logical_bytes,
                    entry
                        .newest_age_days
                        .map_or_else(|| "unknown".into(), |n| n.to_string()),
                    entry.complete,
                    entry.entry_kind,
                    serde_json::to_string(&entry.path)?
                );
            }
            for issue in &report.issues {
                println!("Issue: {}", serde_json::to_string(issue)?);
            }
            println!("{}", report.caveat);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
struct Entry {
    path: PathBuf,
    entry_kind: String,
    logical_bytes: u64,
    observed_entries: u64,
    /// Intentionally skipped descendants (symlinks, special files, other
    /// devices). The entry itself stays `complete`: only the top-level path's
    /// own skip sets `skipped_reason`.
    nested_skipped: u64,
    direct_age_days: Option<u64>,
    oldest_age_days: Option<u64>,
    newest_age_days: Option<u64>,
    complete: bool,
    skipped_reason: Option<String>,
}

impl Entry {
    fn stub(path: PathBuf) -> Self {
        Self {
            path,
            entry_kind: "unknown".into(),
            logical_bytes: 0,
            observed_entries: 0,
            nested_skipped: 0,
            direct_age_days: None,
            oldest_age_days: None,
            newest_age_days: None,
            complete: true,
            skipped_reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ReviewItem {
    path: PathBuf,
    reason: String,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    mode: String,
    root: PathBuf,
    scanned_at: String,
    max_entries: u32,
    max_depth: u32,
    older_than_days: Option<u32>,
    limit: u32,
    max_issues: u32,
    sort: String,
    logical_bytes: u64,
    observed_entries: u64,
    discovered_entries: usize,
    complete: bool,
    matching_entries: usize,
    complete_entries: usize,
    incomplete_entries: usize,
    skipped_entries: usize,
    entries: Vec<Entry>,
    review_queue_total: usize,
    review_queue: Vec<ReviewItem>,
    issues_total: usize,
    issues_omitted: usize,
    issues_by_reason: BTreeMap<String, usize>,
    issues: Vec<String>,
    caveat: &'static str,
}

#[cfg(target_os = "linux")]
fn scan(options: &Options) -> Result<Report> {
    use anyhow::Context;
    use std::{
        fs,
        os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
        time::SystemTime,
    };

    // Linux open(2) flags (uapi/asm-generic/fcntl.h): O_DIRECTORY restricts the
    // open to directories, O_NOFOLLOW refuses a trailing symlink component.
    const O_DIRECTORY: i32 = 0o200000;
    const O_NOFOLLOW: i32 = 0o400000;

    fn open_dir(path: &Path) -> std::io::Result<fs::File> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(path)
    }

    fn kind_of(meta: &fs::Metadata) -> &'static str {
        let kind = meta.file_type();
        if kind.is_file() {
            "file"
        } else if kind.is_dir() {
            "directory"
        } else if kind.is_symlink() {
            "symlink"
        } else {
            "other"
        }
    }

    fn age_days(now: SystemTime, meta: &fs::Metadata) -> Option<u64> {
        meta.modified()
            .ok()
            .map(|modified| now.duration_since(modified).unwrap_or_default().as_secs() / 86400)
    }

    struct Walker<'a> {
        device: u64,
        now: SystemTime,
        max_depth: u32,
        remaining: &'a mut u32,
        issues: &'a mut Vec<String>,
        issue_counts: &'a mut BTreeMap<String, usize>,
    }

    impl Walker<'_> {
        fn record(&mut self, key: &str, message: String) {
            *self.issue_counts.entry(key.to_string()).or_insert(0) += 1;
            self.issues.push(message);
        }

        /// Hard failure for this subtree: metadata unreadable, budget or depth
        /// exhausted, or a directory that changed while being opened.
        fn fail(
            &mut self,
            item: &mut Entry,
            path: &Path,
            key: &str,
            detail: impl std::fmt::Display,
        ) {
            item.complete = false;
            self.record(key, format!("{path:?}: {detail}"));
        }

        /// Intentional skip (symlink, special file, other device): the
        /// observation is finished, so `complete` is untouched, but contents
        /// and activity are unknown. Only a skip of the top-level path itself
        /// sets `skipped_reason`; nested skips increment `nested_skipped` so a
        /// repository with ordinary symlinks is not mistaken for a skipped one.
        fn skip(
            &mut self,
            item: &mut Entry,
            path: &Path,
            key: &str,
            reason: &str,
            top_level: bool,
        ) {
            if top_level {
                item.skipped_reason = Some(reason.to_string());
            } else {
                item.nested_skipped += 1;
            }
            self.record(key, format!("{path:?}: {reason}"));
        }

        fn fold_age(&self, item: &mut Entry, age: u64) {
            item.oldest_age_days = Some(item.oldest_age_days.map_or(age, |old| old.max(age)));
            item.newest_age_days = Some(item.newest_age_days.map_or(age, |new| new.min(age)));
        }

        /// Classify one top-level dirent without descending. Directory ages
        /// are the directory's own mtime; sizes cover top-level files only.
        fn inventory_child(&mut self, access: &Path, item: &mut Entry) {
            item.observed_entries = 1;
            let meta = match fs::symlink_metadata(access) {
                Ok(meta) => meta,
                Err(error) => {
                    self.fail(item, &item.path.clone(), "metadata error", error);
                    return;
                }
            };
            item.entry_kind = kind_of(&meta).to_string();
            if meta.dev() != self.device {
                self.skip(
                    item,
                    &item.path.clone(),
                    "filesystem boundary",
                    "skipped filesystem boundary (other device)",
                    true,
                );
                return;
            }
            item.direct_age_days = age_days(self.now, &meta);
            match item.entry_kind.as_str() {
                "file" => {
                    item.logical_bytes = meta.len();
                    if let Some(age) = item.direct_age_days {
                        self.fold_age(item, age);
                    }
                }
                "directory" => {
                    if let Some(age) = item.direct_age_days {
                        self.fold_age(item, age);
                    }
                }
                _ => self.skip(
                    item,
                    &item.path.clone(),
                    "skipped entry",
                    "skipped symlink or special file (not followed)",
                    true,
                ),
            }
        }

        fn visit(&mut self, access: &Path, display: &Path, depth: u32, item: &mut Entry) {
            item.observed_entries += 1;
            let meta = match fs::symlink_metadata(access) {
                Ok(meta) => meta,
                Err(error) => {
                    self.fail(item, display, "metadata error", error);
                    return;
                }
            };
            if item.entry_kind == "unknown" {
                item.entry_kind = kind_of(&meta).to_string();
            }
            if meta.dev() != self.device {
                self.skip(
                    item,
                    display,
                    "filesystem boundary",
                    "skipped filesystem boundary (other device)",
                    depth == 1,
                );
                return;
            }
            if !meta.is_file() && !meta.is_dir() {
                self.skip(
                    item,
                    display,
                    "skipped entry",
                    "skipped symlink or special file (not followed)",
                    depth == 1,
                );
                return;
            }
            // A directory's own mtime counts (e.g. renames), then descendants
            // widen the range. A recent child keeps an old parent out of the
            // age filter via newest_age_days.
            if item.direct_age_days.is_none() {
                if let Some(age) = age_days(self.now, &meta) {
                    item.direct_age_days = Some(age);
                    self.fold_age(item, age);
                }
            } else if let Some(age) = age_days(self.now, &meta) {
                self.fold_age(item, age);
            }
            if meta.is_file() {
                item.logical_bytes = item.logical_bytes.saturating_add(meta.len());
                return;
            }
            if depth > self.max_depth {
                self.fail(
                    item,
                    display,
                    "depth limit",
                    "directory depth limit reached",
                );
                return;
            }
            let dir = match open_dir(access) {
                Ok(dir) => dir,
                Err(error) => {
                    self.fail(item, display, "open error", error);
                    return;
                }
            };
            match dir.metadata() {
                Ok(opened) if opened.dev() == meta.dev() && opened.ino() == meta.ino() => {}
                _ => {
                    self.fail(
                        item,
                        display,
                        "changed during scan",
                        "directory changed while opening",
                    );
                    return;
                }
            }
            // Anchor descendant lookup to the held directory, not a mutable
            // ancestor path, so a concurrent rename cannot redirect the scan.
            let fd_path = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
            let mut children = match fs::read_dir(&fd_path) {
                Ok(children) => children.peekable(),
                Err(error) => {
                    self.fail(item, display, "enumeration error", error);
                    return;
                }
            };
            // Peek first: a budget spent exactly to EOF is a complete scan,
            // not an exhausted one.
            while children.peek().is_some() {
                if *self.remaining == 0 {
                    self.fail(
                        item,
                        display,
                        "entry budget",
                        "entry budget reached; enumeration may be incomplete",
                    );
                    break;
                }
                let Some(child) = children.next() else {
                    break;
                };
                *self.remaining -= 1;
                match child {
                    Ok(child) => self.visit(
                        &child.path(),
                        &display.join(child.file_name()),
                        depth + 1,
                        item,
                    ),
                    Err(error) => self.fail(item, display, "enumeration error", error),
                }
            }
        }
    }

    // Resolve every supplied component through held directory handles so a
    // symlink anywhere in --path (including a trailing slash) is rejected
    // instead of followed.
    let mut root = open_dir(Path::new(if options.path.is_absolute() {
        "/"
    } else {
        "."
    }))?;
    for component in options.path.components() {
        if matches!(
            component,
            std::path::Component::RootDir | std::path::Component::CurDir
        ) {
            continue;
        }
        let access = PathBuf::from(format!("/proc/self/fd/{}", root.as_raw_fd()))
            .join(component.as_os_str());
        root = open_dir(&access).with_context(|| {
            format!(
                "cannot open directory {:?} (must be a directory; symlink components are rejected)",
                options.path
            )
        })?;
    }
    let device = root.metadata()?.dev();
    let now = SystemTime::now();
    let mut report = Report {
        schema_version: 2,
        mode: if options.inventory {
            "inventory".into()
        } else {
            "subtree".into()
        },
        root: options.path.clone(),
        scanned_at: chrono::Utc::now().to_rfc3339(),
        max_entries: options.max_entries,
        max_depth: options.max_depth,
        older_than_days: options.older_than_days,
        limit: options.limit,
        max_issues: options.max_issues,
        sort: options.sort.to_string(),
        logical_bytes: 0,
        observed_entries: 0,
        discovered_entries: 0,
        complete: true,
        matching_entries: 0,
        complete_entries: 0,
        incomplete_entries: 0,
        skipped_entries: 0,
        entries: Vec::new(),
        review_queue_total: 0,
        review_queue: Vec::new(),
        issues_total: 0,
        issues_omitted: 0,
        issues_by_reason: BTreeMap::new(),
        issues: Vec::new(),
        caveat: "Live metadata scan, not a snapshot or deletion authorization. Logical file bytes are not reclaimable disk space; hardlinks count per pathname. Symlinks, other devices and special files are skipped. Partial scans are lower bounds; bounded traversal order is filesystem-dependent. Modification age does not prove inactivity.",
    };
    let mut remaining = options.max_entries;

    // Discover every top-level sibling first so one huge directory cannot hide
    // the rest of the root when the budget runs out mid-descent.
    let root_fd_path = format!("/proc/self/fd/{}", root.as_raw_fd());
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    let mut root_children = fs::read_dir(root_fd_path)
        .context("cannot enumerate scratch root")?
        .peekable();
    while root_children.peek().is_some() {
        if remaining == 0 {
            report.complete = false;
            *report
                .issues_by_reason
                .entry("entry budget".to_string())
                .or_insert(0) += 1;
            report
                .issues
                .push("Entry budget reached; root enumeration may be incomplete".to_string());
            break;
        }
        let Some(child) = root_children.next() else {
            break;
        };
        remaining -= 1;
        match child {
            Ok(child) => names.push(child.file_name()),
            Err(error) => {
                report.complete = false;
                *report
                    .issues_by_reason
                    .entry("enumeration error".to_string())
                    .or_insert(0) += 1;
                report.issues.push(error.to_string());
            }
        }
    }
    names.sort();
    report.discovered_entries = names.len();

    let mut all: Vec<Entry> = Vec::with_capacity(names.len());
    for name in &names {
        let path = options.path.join(name);
        let Some(path_str) = path.to_str() else {
            report.complete = false;
            *report
                .issues_by_reason
                .entry("non-UTF-8 path".to_string())
                .or_insert(0) += 1;
            report
                .issues
                .push(format!("Skipped non-UTF-8 top-level path: {path:?}"));
            report.skipped_entries += 1;
            continue;
        };
        let _ = path_str;
        let mut item = Entry::stub(path);
        // Re-resolve the child through the held root handle instead of trusting
        // the joined display path, closing a rename race between listing and
        // visiting.
        let access = PathBuf::from(format!("/proc/self/fd/{}", root.as_raw_fd())).join(name);
        let mut walker = Walker {
            device,
            now,
            max_depth: options.max_depth,
            remaining: &mut remaining,
            issues: &mut report.issues,
            issue_counts: &mut report.issues_by_reason,
        };
        if options.inventory {
            walker.inventory_child(&access, &mut item);
        } else {
            walker.visit(&access, &item.path.clone(), 1, &mut item);
        }
        report.logical_bytes = report.logical_bytes.saturating_add(item.logical_bytes);
        report.observed_entries += item.observed_entries;
        report.complete &= item.complete;
        all.push(item);
    }

    for item in &all {
        if item.complete {
            report.complete_entries += 1;
        } else {
            report.incomplete_entries += 1;
        }
        if item.skipped_reason.is_some() {
            report.skipped_entries += 1;
        }
    }

    // Unknown or unfinished entries get explicit human attention instead of a
    // confident ranking or silent exclusion.
    let mut review_queue: Vec<ReviewItem> = all
        .iter()
        .filter_map(|item| {
            if !item.complete {
                Some(ReviewItem {
                    path: item.path.clone(),
                    reason: item
                        .skipped_reason
                        .clone()
                        .map(|reason| format!("incomplete scan; {reason}"))
                        .unwrap_or_else(|| "incomplete scan".to_string()),
                })
            } else if let Some(reason) = &item.skipped_reason {
                Some(ReviewItem {
                    path: item.path.clone(),
                    reason: format!("skipped ({reason}); contents and activity unknown"),
                })
            } else if item.newest_age_days.is_none() {
                Some(ReviewItem {
                    path: item.path.clone(),
                    reason: "unknown modification age".to_string(),
                })
            } else {
                None
            }
        })
        .collect();
    review_queue.sort_by(|a, b| a.path.cmp(&b.path));
    report.review_queue_total = review_queue.len();
    review_queue.truncate(options.limit as usize);
    report.review_queue = review_queue;

    // Age filters apply to newest observed activity and only to complete
    // entries with known ages; everything else stays visible via review_queue.
    let mut matching: Vec<Entry> = all
        .into_iter()
        .filter(|item| {
            options.older_than_days.is_none_or(|days| {
                item.complete
                    && item
                        .newest_age_days
                        .is_some_and(|age| age >= u64::from(days))
            })
        })
        .collect();
    match options.sort {
        SortMode::Size => matching.sort_by(|a, b| {
            b.logical_bytes
                .cmp(&a.logical_bytes)
                .then_with(|| a.path.cmp(&b.path))
        }),
        SortMode::Oldest => matching.sort_by(|a, b| {
            let rank = |entry: &Entry| {
                if entry.complete && entry.newest_age_days.is_some() {
                    0
                } else {
                    1
                }
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| b.newest_age_days.cmp(&a.newest_age_days))
                .then_with(|| a.path.cmp(&b.path))
        }),
    }
    report.matching_entries = matching.len();
    matching.truncate(options.limit as usize);
    report.entries = matching;

    report.issues_total = report.issues.len();
    if report.issues.len() > options.max_issues as usize {
        report.issues.truncate(options.max_issues as usize);
    }
    report.issues_omitted = report.issues_total - report.issues.len();
    Ok(report)
}

#[cfg(not(target_os = "linux"))]
fn scan(options: &Options) -> Result<Report> {
    let _ = options;
    anyhow::bail!(
        "`doty analyze opencode` requires Linux (O_NOFOLLOW directory handles and /proc/self/fd); refusing to run a weaker scan on {}",
        std::env::consts::OS
    )
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "linux"))]
    use super::{Options, PathBuf, SortMode, scan};

    #[test]
    fn cli_rejects_mutation_and_unbounded_options() {
        use clap::Parser;
        for args in [
            vec!["doty", "analyze", "opencode", "--apply"],
            vec!["doty", "analyze", "opencode", "--max-entries", "0"],
            vec!["doty", "analyze", "opencode", "--max-depth", "0"],
            vec!["doty", "analyze", "opencode", "--max-depth", "129"],
            vec!["doty", "analyze", "opencode", "--limit", "0"],
            vec!["doty", "analyze", "opencode", "--sort", "everything"],
        ] {
            assert!(crate::cli::Cli::try_parse_from(args).is_err());
        }
        for args in [
            vec!["doty", "analyze", "opencode", "--inventory"],
            vec![
                "doty",
                "analyze",
                "opencode",
                "--sort",
                "oldest",
                "--max-issues",
                "0",
            ],
        ] {
            assert!(crate::cli::Cli::try_parse_from(args).is_ok());
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_refuses_scan_rather_than_weakening_it() {
        let options = Options {
            path: PathBuf::from("/tmp"),
            json: true,
            older_than_days: None,
            limit: 50,
            max_entries: 100,
            max_depth: 64,
            inventory: false,
            sort: SortMode::Size,
            max_issues: 50,
        };
        assert!(scan(&options).is_err());
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::super::{Options, Path, PathBuf, Result, SortMode, scan};
        use std::{
            fs,
            os::unix::{ffi::OsStringExt, fs::symlink},
            time::{Duration, SystemTime},
        };

        fn options(path: PathBuf) -> Options {
            Options {
                path,
                json: true,
                older_than_days: None,
                limit: 50,
                max_entries: 100_000,
                max_depth: 64,
                inventory: false,
                sort: SortMode::Size,
                max_issues: 50,
            }
        }

        fn set_age(path: &Path, days: u64) {
            let time = SystemTime::now() - Duration::from_secs(days * 86400);
            fs::File::open(path).unwrap().set_modified(time).unwrap();
        }

        fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("secret"), "not inspected").unwrap();
            fs::create_dir(root.path().join("tree")).unwrap();
            fs::write(root.path().join("tree/recent"), "12345").unwrap();
            fs::write(root.path().join("a"), "123").unwrap();
            fs::write(root.path().join("b"), "123").unwrap();
            // Directory mtimes would otherwise mask the recent child.
            for path in ["tree", "a", "b"] {
                set_age(&root.path().join(path), 30);
            }
            (root, outside)
        }

        #[test]
        fn subtree_scan_ranks_sizes_and_filters_by_age() -> Result<()> {
            let (root, _outside) = fixture();
            let mut opts = options(root.path().into());
            let report = scan(&opts)?;
            assert_eq!(report.schema_version, 2);
            assert_eq!(report.mode, "subtree");
            assert!(report.complete);
            assert_eq!(report.logical_bytes, 11);
            assert_eq!(report.observed_entries, 4);
            assert_eq!(report.discovered_entries, 3);
            assert_eq!(report.issues_total, 0);
            assert_eq!(report.review_queue_total, 0);
            assert!(report.entries[0].path.ends_with("tree"));
            assert_eq!(report.entries[0].newest_age_days, Some(0));
            assert!(report.entries[1].path.ends_with("a"));
            assert!(report.entries[2].path.ends_with("b"));

            opts.older_than_days = Some(14);
            let aged = scan(&opts)?;
            assert_eq!(aged.entries.len(), 2);
            assert_eq!(aged.matching_entries, 2);
            // Totals still describe the whole scan, not the filter window.
            assert_eq!(aged.logical_bytes, 11);

            opts.older_than_days = None;
            opts.limit = 1;
            let limited = scan(&opts)?;
            assert_eq!(limited.matching_entries, 3);
            assert_eq!(limited.entries.len(), 1);

            // The scan reads metadata only: contents and mtimes are untouched.
            assert_eq!(fs::read(root.path().join("tree/recent"))?, b"12345");
            Ok(())
        }

        #[test]
        fn symlinks_are_skipped_without_traversal() -> Result<()> {
            let (root, outside) = fixture();
            fs::write(outside.path().join("secret"), "still not inspected")?;
            std::os::unix::fs::symlink(outside.path(), root.path().join("external"))?;
            std::os::unix::fs::symlink(root.path(), root.path().join("tree/cycle"))?;
            let opts = options(root.path().into());
            let report = scan(&opts)?;
            // Intentional skips are finished observations, not failures.
            assert!(report.complete);
            assert_eq!(report.logical_bytes, 11);
            assert_eq!(report.issues_total, 2);
            assert_eq!(report.issues_by_reason.get("skipped entry"), Some(&2));
            let external = report
                .entries
                .iter()
                .find(|entry| entry.path.ends_with("external"))
                .unwrap();
            assert_eq!(external.entry_kind, "symlink");
            assert!(external.complete);
            assert!(external.newest_age_days.is_none());
            assert!(external.skipped_reason.is_some());
            // The nested cycle is counted on its subtree, not mistaken for a
            // skipped top-level entry: `tree` keeps its ages and stays out of
            // the review queue.
            let tree = report
                .entries
                .iter()
                .find(|entry| entry.path.ends_with("tree"))
                .unwrap();
            assert!(tree.complete);
            assert!(tree.skipped_reason.is_none());
            assert_eq!(tree.nested_skipped, 1);
            assert_eq!(tree.newest_age_days, Some(0));
            // Skipped entries stay visible via the review queue, never ranked
            // as confidently old.
            assert_eq!(report.review_queue_total, 1);
            assert!(report.review_queue[0].path.ends_with("external"));
            assert_eq!(
                fs::read(outside.path().join("secret"))?,
                b"still not inspected"
            );
            Ok(())
        }

        #[test]
        fn budget_exhaustion_marks_incomplete() -> Result<()> {
            let (root, _outside) = fixture();
            let mut opts = options(root.path().into());
            opts.max_entries = 1;
            let report = scan(&opts)?;
            assert!(!report.complete);
            // Root-listing order decides whether the single budget token lands
            // on a file or on `tree` (whose descent then fails the budget a
            // second time), so only the property is asserted, not the count.
            assert!(report.issues_by_reason.get("entry budget").unwrap_or(&0) >= &1);
            assert!(report.incomplete_entries >= 1);
            assert!(report.review_queue_total >= 1);
            Ok(())
        }

        #[test]
        fn depth_limit_marks_incomplete_without_other_failures() -> Result<()> {
            let (root, _outside) = fixture();
            fs::create_dir(root.path().join("tree/deep"))?;
            fs::write(root.path().join("tree/deep/file"), "hidden by depth limit")?;
            let mut opts = options(root.path().into());
            opts.max_depth = 1;
            let report = scan(&opts)?;
            assert!(!report.complete);
            assert_eq!(report.issues_total, 1);
            assert_eq!(report.issues_by_reason.get("depth limit"), Some(&1));
            Ok(())
        }

        #[test]
        fn inventory_lists_every_sibling_despite_huge_first_child() -> Result<()> {
            let (root, _outside) = fixture();
            fs::create_dir(root.path().join("aaa-huge")).unwrap();
            for n in 0..50 {
                fs::write(root.path().join(format!("aaa-huge/f{n}")), "x").unwrap();
            }
            let mut opts = options(root.path().into());
            opts.inventory = true;
            opts.max_entries = 4;
            let report = scan(&opts)?;
            assert_eq!(report.mode, "inventory");
            assert!(report.complete);
            // All four siblings discovered; no descent into the huge child.
            assert_eq!(report.discovered_entries, 4);
            let huge = report
                .entries
                .iter()
                .find(|entry| entry.path.ends_with("aaa-huge"))
                .unwrap();
            assert_eq!(huge.entry_kind, "directory");
            assert_eq!(huge.observed_entries, 1);
            assert_eq!(huge.logical_bytes, 0);
            Ok(())
        }

        #[test]
        fn oldest_sort_puts_unknown_last_and_stays_deterministic() -> Result<()> {
            let (root, _outside) = fixture();
            set_age(&root.path().join("b"), 60);
            symlink(root.path().join("a"), root.path().join("link"))?;
            let mut opts = options(root.path().into());
            opts.sort = SortMode::Oldest;
            let first = scan(&opts)?;
            let second = scan(&opts)?;
            let names: Vec<_> = first
                .entries
                .iter()
                .map(|entry| entry.path.file_name().unwrap().to_owned())
                .collect();
            assert_eq!(
                names,
                second
                    .entries
                    .iter()
                    .map(|entry| entry.path.file_name().unwrap().to_owned())
                    .collect::<Vec<_>>()
            );
            // b (60d) before tree/a (30d-era activity), unknown symlink last.
            let position = |name: &str| names.iter().position(|n| n == name).unwrap();
            assert!(position("b") < position("a"));
            assert!(position("a") < position("link"));
            Ok(())
        }

        #[test]
        fn issues_are_bounded_but_counts_survive() -> Result<()> {
            let (root, _outside) = fixture();
            for n in 0..5 {
                symlink(root.path().join("a"), root.path().join(format!("l{n}")))?;
            }
            let mut opts = options(root.path().into());
            opts.max_issues = 2;
            let report = scan(&opts)?;
            assert_eq!(report.issues_total, 5);
            assert_eq!(report.issues.len(), 2);
            assert_eq!(report.issues_omitted, 3);
            assert_eq!(report.issues_by_reason.get("skipped entry"), Some(&5));
            Ok(())
        }

        #[test]
        fn non_utf8_names_are_reported_not_silent() -> Result<()> {
            use std::ffi::OsString;
            let (root, _outside) = fixture();
            let raw = OsString::from_vec(b"bad-\xff-name".to_vec());
            fs::write(root.path().join(&raw), "x")?;
            let opts = options(root.path().into());
            let report = scan(&opts)?;
            assert!(!report.complete);
            assert_eq!(report.issues_by_reason.get("non-UTF-8 path"), Some(&1));
            assert_eq!(report.skipped_entries, 1);
            Ok(())
        }

        #[test]
        fn root_errors_are_hard_failures() -> Result<()> {
            let (root, _outside) = fixture();
            symlink(root.path().join("a"), root.path().join("link"))?;
            for path in [
                root.path().join("missing"),
                root.path().join("link"),
                root.path().join("link/"),
                root.path().join("link/subdir"),
                root.path().join("a"),
            ] {
                let opts = options(path);
                assert!(scan(&opts).is_err());
            }
            Ok(())
        }
    }
}
