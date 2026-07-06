use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const SCAN_MAX_ENTRIES: u64 = 50_000;

struct CanixPreflightCacheFramework;

impl Framework for CanixPreflightCacheFramework {
    fn name(&self) -> &'static str {
        "canix-preflight-cache"
    }

    fn summary(&self) -> &'static str {
        "Canix cargo preflight project cache"
    }

    fn variants(&self) -> &[&'static dyn Variant] {
        &[&SizeCap]
    }
}

static FRAMEWORK: CanixPreflightCacheFramework = CanixPreflightCacheFramework;

pub static CANIX_PREFLIGHT_CACHE: &dyn Framework = &FRAMEWORK;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    #[serde(default = "default_max_bytes")]
    max_bytes: u64,
}

fn default_max_bytes() -> u64 {
    DEFAULT_MAX_BYTES
}

impl Settings {
    fn from_value(value: &Value) -> Result<Self> {
        if value.is_null() {
            return Ok(Self {
                max_bytes: DEFAULT_MAX_BYTES,
            });
        }
        serde_json::from_value(value.clone()).map_err(Into::into)
    }
}

struct SizeCap;

impl Variant for SizeCap {
    fn name(&self) -> &'static str {
        "size-cap"
    }

    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }

    fn tier(&self) -> Tier {
        Tier::Safe
    }

    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&Value::Null)
    }

    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        let settings = Settings::from_value(settings)?;
        let plan = cleanup_plan(settings.max_bytes);
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/home/<users>/.cache/canix/preflight".into(),
            size_bytes: Some(plan.remove_bytes),
            age_oldest_days: plan.oldest_age_days,
            would_remove: plan.remove.len() as u64,
            notes: plan.notes(settings.max_bytes),
        })
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
        let plan = cleanup_plan(settings.max_bytes);
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: plan.remove.len() as u64,
                errors: vec![format!(
                    "dry-run: would remove {} canix preflight project caches ({})",
                    plan.remove.len(),
                    fmt_bytes(plan.remove_bytes)
                )],
            });
        }

        let mut removed = 0u64;
        let mut freed_bytes = 0u64;
        let mut errors = Vec::new();
        for entry in plan.remove {
            match fs::remove_dir_all(&entry.path) {
                Ok(()) => {
                    removed += 1;
                    freed_bytes = freed_bytes.saturating_add(entry.bytes);
                }
                Err(e) => errors.push(format!("cannot remove {}: {e}", entry.path.display())),
            }
        }

        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes,
            skipped: 0,
            errors,
        })
    }
}

#[derive(Debug, Clone)]
struct CacheEntry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

#[derive(Debug, Clone, Default)]
struct CleanupPlan {
    total_bytes: u64,
    remove_bytes: u64,
    remove: Vec<CacheEntry>,
    oldest_age_days: Option<u32>,
    truncated: bool,
}

impl CleanupPlan {
    fn notes(&self, max_bytes: u64) -> String {
        let names = self
            .remove
            .iter()
            .take(8)
            .filter_map(|entry| entry.path.file_name())
            .map(|name| name.to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if self.truncated {
            " (scan truncated)"
        } else {
            ""
        };
        if names.is_empty() {
            format!(
                "canix preflight cache total {} within cap {}{suffix}",
                fmt_bytes(self.total_bytes),
                fmt_bytes(max_bytes)
            )
        } else {
            format!(
                "canix preflight cache total {}, cap {}; remove oldest: {names}{suffix}",
                fmt_bytes(self.total_bytes),
                fmt_bytes(max_bytes)
            )
        }
    }
}

fn cleanup_plan(max_bytes: u64) -> CleanupPlan {
    let mut entries = cache_entries();
    let mut plan = CleanupPlan {
        total_bytes: entries.iter().map(|entry| entry.bytes).sum(),
        ..Default::default()
    };

    if plan.total_bytes <= max_bytes {
        return plan;
    }

    entries.sort_by_key(|entry| entry.modified);
    let mut remaining = plan.total_bytes;
    for entry in entries {
        if remaining <= max_bytes {
            break;
        }
        remaining = remaining.saturating_sub(entry.bytes);
        plan.remove_bytes = plan.remove_bytes.saturating_add(entry.bytes);
        plan.oldest_age_days = plan.oldest_age_days.max(Some(age_days(entry.modified)));
        plan.remove.push(entry);
    }

    plan
}

fn cache_entries() -> Vec<CacheEntry> {
    let mut entries = Vec::new();
    for root in crate::exec::all_user_subdirs(".cache/canix/preflight") {
        let Ok(children) = fs::read_dir(&root) else {
            continue;
        };
        for child in children.flatten() {
            let path = child.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_dir() {
                continue;
            }
            let scan =
                crate::exec::total_dir_size_bounded(&path.to_string_lossy(), SCAN_MAX_ENTRIES)
                    .unwrap_or_default();
            entries.push(CacheEntry {
                path,
                bytes: scan.bytes,
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }
    entries
}

fn age_days(modified: SystemTime) -> u32 {
    let modified_utc: chrono::DateTime<chrono::Utc> = modified.into();
    let days = chrono::Utc::now()
        .signed_duration_since(modified_utc)
        .num_days()
        .max(0);
    u32::try_from(days).unwrap_or(u32::MAX)
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
