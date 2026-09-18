use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

const DEFAULT_ROOT: &str = "/data/scratch/tmp/opencode";
const SIZE_BUDGET_PER_PATH: u64 = 200_000;

struct OpencodeScratchFramework;

impl Framework for OpencodeScratchFramework {
    fn name(&self) -> &'static str {
        "opencode-scratch"
    }

    fn summary(&self) -> &'static str {
        "Opencode agent scratch: regenerable cargo target dirs under explicit allowlist"
    }

    fn variants(&self) -> &[&'static dyn Variant] {
        &[&PurgeAllowlistedTargets]
    }
}

static FRAMEWORK: OpencodeScratchFramework = OpencodeScratchFramework;

pub static OPENCODE_SCRATCH: &dyn Framework = &FRAMEWORK;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    #[serde(default = "default_root")]
    root: PathBuf,
    #[serde(default)]
    dirs: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default = "default_require_proof")]
    require_proof: bool,
    /// Entries modified more recently than this are skipped. Cheap
    /// active-build protection: compilers keep target dirs hot while
    /// running, so a one-hour idle window refuses mid-build deletion
    /// without process scanning.
    #[serde(default = "default_min_idle_minutes")]
    min_idle_minutes: u32,
}

fn default_root() -> PathBuf {
    PathBuf::from(DEFAULT_ROOT)
}

fn default_require_proof() -> bool {
    true
}

fn default_min_idle_minutes() -> u32 {
    60
}

impl Settings {
    fn from_value(value: &Value) -> Result<Self> {
        if value.is_null() {
            return Ok(Self {
                root: default_root(),
                dirs: Vec::new(),
                files: Vec::new(),
                require_proof: default_require_proof(),
                min_idle_minutes: default_min_idle_minutes(),
            });
        }
        serde_json::from_value(value.clone()).map_err(Into::into)
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    bytes: u64,
    age_days: u32,
    truncated: bool,
}

#[derive(Debug, Clone)]
struct Skipped {
    path: String,
    reason: &'static str,
}

/// Resolve an allowlist entry against the root. Anything outside the root,
/// any symlink, and any git checkout is refused: only plain dirs (for
/// `dirs`) and plain files (for `files`) become candidates.
fn resolve_entry(root: &Path, rel: &str, want_dir: bool) -> Result<PathBuf, Skipped> {
    let rel_path = Path::new(rel);
    if rel.is_empty() || rel_path.is_absolute() {
        return Err(Skipped {
            path: rel.to_string(),
            reason: "not a relative path",
        });
    }
    if rel_path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Skipped {
            path: rel.to_string(),
            reason: "escapes the scratch root",
        });
    }
    let full = root.join(rel_path);
    let meta = fs::symlink_metadata(&full).map_err(|_| Skipped {
        path: full.display().to_string(),
        reason: "missing",
    })?;
    if meta.file_type().is_symlink() {
        return Err(Skipped {
            path: full.display().to_string(),
            reason: "symlink (never followed)",
        });
    }
    if want_dir && !meta.is_dir() {
        return Err(Skipped {
            path: full.display().to_string(),
            reason: "not a directory",
        });
    }
    if !want_dir && !meta.is_file() {
        return Err(Skipped {
            path: full.display().to_string(),
            reason: "not a file",
        });
    }
    if full.join(".git").exists() {
        return Err(Skipped {
            path: full.display().to_string(),
            reason: "git checkout (worktree track only)",
        });
    }
    Ok(full)
}

/// Regenerability proof: a cargo target dir carries CACHEDIR.TAG or a
/// debug/.fingerprint cache. Tag-absent dirs are only removable when the
/// caller sets requireProof to false.
fn has_proof(dir: &Path) -> bool {
    dir.join("CACHEDIR.TAG").is_file() || dir.join("debug").join(".fingerprint").is_dir()
}

fn resolve_candidates(settings: &Settings) -> (Vec<Candidate>, Vec<Skipped>) {
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    let mut seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for rel in &settings.dirs {
        match resolve_entry(&settings.root, rel, true) {
            Ok(path) => {
                if !seen.insert(path.clone()) {
                    continue;
                }
                if settings.require_proof && !has_proof(&path) {
                    skipped.push(Skipped {
                        path: path.display().to_string(),
                        reason: "no CACHEDIR.TAG or debug/.fingerprint proof",
                    });
                    continue;
                }
                if idle_minutes_of(&path) < settings.min_idle_minutes {
                    skipped.push(Skipped {
                        path: path.display().to_string(),
                        reason: "modified within idle window (possibly building)",
                    });
                    continue;
                }
                let scan = crate::exec::total_dir_size_bounded(
                    &path.display().to_string(),
                    SIZE_BUDGET_PER_PATH,
                )
                .unwrap_or_default();
                candidates.push(Candidate {
                    bytes: scan.bytes,
                    age_days: age_days_of(&path),
                    truncated: scan.truncated,
                    path,
                });
            }
            Err(skip) => skipped.push(skip),
        }
    }
    for rel in &settings.files {
        match resolve_entry(&settings.root, rel, false) {
            Ok(path) => {
                if !seen.insert(path.clone()) {
                    continue;
                }
                if idle_minutes_of(&path) < settings.min_idle_minutes {
                    skipped.push(Skipped {
                        path: path.display().to_string(),
                        reason: "modified within idle window (possibly building)",
                    });
                    continue;
                }
                candidates.push(Candidate {
                    bytes: fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                    age_days: age_days_of(&path),
                    truncated: false,
                    path,
                });
            }
            Err(skip) => skipped.push(skip),
        }
    }
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    (candidates, skipped)
}

fn age_days_of(path: &Path) -> u32 {
    idle_to_days(idle_minutes_of(path))
}

fn idle_minutes_of(path: &Path) -> u32 {
    let modified = fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let modified_utc: chrono::DateTime<chrono::Utc> = modified.into();
    chrono::Utc::now()
        .signed_duration_since(modified_utc)
        .num_minutes()
        .max(0)
        .try_into()
        .unwrap_or(u32::MAX)
}

fn idle_to_days(minutes: u32) -> u32 {
    minutes / 1440
}

fn inspection(
    variant: &dyn Variant,
    root: &Path,
    candidates: &[Candidate],
    skipped: &[Skipped],
) -> Inspection {
    let bytes = candidates.iter().map(|c| c.bytes).sum();
    let oldest = candidates.iter().map(|c| c.age_days).max();
    let count = candidates.len() as u64;
    let bounded_note = if candidates.iter().any(|c| c.truncated) {
        ", sizes are lower bounds (scan budget)"
    } else {
        ""
    };
    Inspection {
        framework: variant.framework().name(),
        variant: variant.name(),
        path: root.display().to_string(),
        size_bytes: Some(bytes),
        age_oldest_days: oldest,
        would_remove: count,
        notes: format!(
            "{count} allowlisted scratch entries ({}){tail}",
            fmt_bytes(bytes),
            tail = format!("{bounded_note}{}", skipped_note(skipped)),
        ),
    }
}

fn skipped_note(skipped: &[Skipped]) -> String {
    if skipped.is_empty() {
        String::new()
    } else {
        let mut reasons: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for skip in skipped {
            *reasons.entry(skip.reason).or_default() += 1;
        }
        let summary: Vec<String> = reasons
            .into_iter()
            .map(|(reason, n)| format!("{n}x {reason}"))
            .collect();
        format!("; skipped: {}", summary.join(", "))
    }
}

struct PurgeAllowlistedTargets;

impl Variant for PurgeAllowlistedTargets {
    fn name(&self) -> &'static str {
        "purge-allowlisted-targets"
    }

    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }

    fn tier(&self) -> Tier {
        Tier::Confirm
    }

    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&Value::Null)
    }

    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        let settings = Settings::from_value(settings)?;
        let (candidates, skipped) = resolve_candidates(&settings);
        Ok(inspection(self, &settings.root, &candidates, &skipped))
    }

    fn apply(&self, apply: bool, force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(apply, force, &Value::Null)
    }

    fn apply_with_settings(
        &self,
        apply: bool,
        _force: bool,
        settings: &Value,
    ) -> Result<ApplyReport> {
        let settings = Settings::from_value(settings)?;
        let (candidates, skipped) = resolve_candidates(&settings);
        let count = candidates.len() as u64;
        let bytes = candidates.iter().map(|c| c.bytes).sum();
        if !apply {
            let mut errors = vec![format!(
                "dry-run: would remove {count} allowlisted scratch entries ({}){}",
                fmt_bytes(bytes),
                skipped_note(&skipped),
            )];
            for skip in &skipped {
                errors.push(format!("skipped {}: {}", skip.path, skip.reason));
            }
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: count + skipped.len() as u64,
                errors,
            });
        }

        let mut removed = 0;
        let mut freed_bytes: u64 = 0;
        let mut errors = Vec::new();
        for candidate in candidates {
            let outcome = if candidate.path.is_dir() {
                fs::remove_dir_all(&candidate.path).map_err(|e| e.to_string())
            } else {
                fs::remove_file(&candidate.path).map_err(|e| e.to_string())
            };
            match outcome {
                Ok(()) => {
                    removed += 1;
                    freed_bytes = freed_bytes.saturating_add(candidate.bytes);
                }
                Err(e) => errors.push(format!("cannot remove {}: {e}", candidate.path.display())),
            }
        }
        for skip in &skipped {
            errors.push(format!("skipped {}: {}", skip.path, skip.reason));
        }

        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes,
            skipped: skipped.len() as u64,
            errors,
        })
    }
}

fn fmt_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    format!("{:.1} {}", size, UNITS[unit_idx])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn settings_for(root: &Path) -> Value {
        serde_json::json!({
            "root": root.display().to_string(),
            "dirs": ["amc-target", "nested/target"],
            "files": ["notes.txt"],
            "minIdleMinutes": 0,
        })
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("amc-target").join("debug").join(".fingerprint"))
            .unwrap();
        fs::write(dir.path().join("amc-target").join("CACHEDIR.TAG"), "tag").unwrap();
        fs::write(dir.path().join("amc-target").join("junk.bin"), "junk").unwrap();
        fs::create_dir_all(dir.path().join("nested").join("target")).unwrap();
        fs::write(
            dir.path().join("nested").join("target").join("out.o"),
            "object",
        )
        .unwrap();
        fs::write(dir.path().join("notes.txt"), "notes").unwrap();
        fs::write(dir.path().join("unlisted.txt"), "keep").unwrap();
        dir
    }

    #[test]
    fn settings_parse_allowlist_and_proof_flag() {
        let settings = Settings::from_value(&serde_json::json!({
            "root": "/tmp/scratch",
            "dirs": ["a-target"],
            "files": ["f.txt"],
            "requireProof": false,
            "minIdleMinutes": 5,
        }))
        .unwrap();

        assert_eq!(settings.root, PathBuf::from("/tmp/scratch"));
        assert_eq!(settings.dirs, vec!["a-target".to_string()]);
        assert_eq!(settings.files, vec!["f.txt".to_string()]);
        assert!(!settings.require_proof);
        assert_eq!(settings.min_idle_minutes, 5);
    }

    #[test]
    fn settings_default_to_empty_allowlist_with_proof() {
        let settings = Settings::from_value(&Value::Null).unwrap();

        assert_eq!(settings.root, PathBuf::from(DEFAULT_ROOT));
        assert!(settings.dirs.is_empty());
        assert!(settings.files.is_empty());
        assert!(settings.require_proof);
        assert_eq!(settings.min_idle_minutes, 60);
    }

    #[test]
    fn dry_run_lists_candidates_and_removes_nothing() {
        let dir = fixture();
        let report = PurgeAllowlistedTargets
            .apply_with_settings(false, false, &settings_for(dir.path()))
            .unwrap();

        // nested/target lacks proof files and requireProof defaults to true.
        assert_eq!(report.removed, 0);
        assert!(dir.path().join("amc-target").exists());
        assert!(dir.path().join("notes.txt").exists());
        assert!(dir.path().join("unlisted.txt").exists());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("no CACHEDIR.TAG")),
            "unproven dir must be reported, got: {:?}",
            report.errors
        );
    }

    #[test]
    fn apply_removes_only_resolved_candidates() {
        let dir = fixture();
        let report = PurgeAllowlistedTargets
            .apply_with_settings(
                true,
                true,
                &serde_json::json!({
                    "root": dir.path().display().to_string(),
                    "dirs": ["amc-target", "nested/target", "missing-target"],
                    "files": ["notes.txt"],
                    "requireProof": false,
                    "minIdleMinutes": 0,
                }),
            )
            .unwrap();

        assert_eq!(report.removed, 3);
        assert!(!dir.path().join("amc-target").exists());
        assert!(!dir.path().join("nested").join("target").exists());
        assert!(!dir.path().join("notes.txt").exists());
        assert!(dir.path().join("unlisted.txt").exists());
        assert!(
            report.errors.iter().any(|e| e.contains("missing")),
            "missing entry must be reported, got: {:?}",
            report.errors
        );
    }

    #[test]
    fn idle_window_skips_fresh_entries() {
        let dir = fixture();
        let report = PurgeAllowlistedTargets
            .apply_with_settings(
                false,
                false,
                &serde_json::json!({
                    "root": dir.path().display().to_string(),
                    "dirs": ["amc-target"],
                    "files": ["notes.txt"],
                    "requireProof": false,
                }),
            )
            .unwrap();

        // Fixtures were just created, so the default 60-minute idle window
        // refuses them instead of deleting.
        assert_eq!(report.removed, 0);
        assert!(dir.path().join("amc-target").exists());
        assert!(
            report.errors.iter().any(|e| e.contains("idle window")),
            "fresh entries must be reported, got: {:?}",
            report.errors
        );
    }

    #[test]
    fn scope_lock_rejects_escape_absolute_and_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, dir.path().join("linked")).unwrap();

        for rel in ["../outside", "/tmp/absolute", "linked"] {
            let err = resolve_entry(dir.path(), rel, true).unwrap_err();
            assert!(
                !matches!(err.reason, "missing"),
                "{rel} must be refused for its own reason, got missing"
            );
        }
        assert_eq!(
            resolve_entry(dir.path(), "../outside", true).unwrap_err().reason,
            "escapes the scratch root"
        );
        assert_eq!(
            resolve_entry(dir.path(), "/tmp/absolute", true)
                .unwrap_err()
                .reason,
            "not a relative path"
        );
        #[cfg(unix)]
        assert_eq!(
            resolve_entry(dir.path(), "linked", true).unwrap_err().reason,
            "symlink (never followed)"
        );
    }

    #[test]
    fn git_checkouts_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("checkout");
        fs::create_dir_all(repo.join(".git")).unwrap();

        assert_eq!(
            resolve_entry(dir.path(), "checkout", true)
                .unwrap_err()
                .reason,
            "git checkout (worktree track only)"
        );
    }
}
