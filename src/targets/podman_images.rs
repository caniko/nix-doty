use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_KEEP_SINCE_HOURS: u32 = 168;

struct PodmanImagesFramework;

impl Framework for PodmanImagesFramework {
    fn name(&self) -> &'static str {
        "podman-images"
    }
    fn summary(&self) -> &'static str {
        "Podman container image storage"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&DiskReport, &PruneInactiveOlder]
    }
}

static FRAMEWORK: PodmanImagesFramework = PodmanImagesFramework;

pub static PODMAN_IMAGES: &dyn Framework = &FRAMEWORK;

#[derive(Debug, Clone, Deserialize)]
struct PodmanImage {
    #[serde(default)]
    size: u64,
}

fn keep_since_hours(settings: &Value) -> u32 {
    settings
        .get("keepSinceHours")
        .and_then(Value::as_u64)
        .map(|v| v as u32)
        .unwrap_or(DEFAULT_KEEP_SINCE_HOURS)
}

fn list_images() -> Vec<PodmanImage> {
    let out = exec::run_stdout(&[
        "podman", "images", "--all", "--format", "json",
    ]);
    match out {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn prune_image_count(hours: u32) -> u64 {
    let out = exec::run_stdout(&[
        "podman", "image", "prune", "--all",
        "--filter", &format!("until={hours}h"),
        "--force",
    ]);
    match out {
        Ok(text) => {
            text.lines()
                .filter(|l| l.contains("deleted") || l.contains("untagged"))
                .count() as u64
        }
        Err(_) => 0,
    }
}

struct DiskReport;
impl Variant for DiskReport {
    fn name(&self) -> &'static str {
        "disk-report"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::ReportOnly
    }
    fn inspect(&self) -> Result<Inspection> {
        let images = list_images();
        let count = images.len() as u64;
        let total_size: u64 = images.iter().map(|i| i.size).sum();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/var/lib/containers/storage".into(),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: 0,
            notes: format!("{count} podman images ({:.1} GiB)", total_size as f64 / 1073741824.0),
        })
    }
    fn apply(&self, _apply: bool, _force: bool) -> Result<ApplyReport> {
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 0,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
}

struct PruneInactiveOlder;
impl Variant for PruneInactiveOlder {
    fn name(&self) -> &'static str {
        "prune-inactive-older"
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
        let hours = keep_since_hours(settings);
        let images = list_images();
        let count = images.len() as u64;
        let total_size: u64 = images.iter().map(|i| i.size).sum();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/var/lib/containers/storage".into(),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: count,
            notes: format!(
                "{count} images ({:.1} GiB) — would prune those inactive >{hours}h",
                total_size as f64 / 1073741824.0
            ),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(apply, _force, &Value::Null)
    }
    fn apply_with_settings(&self, apply: bool, _force: bool, settings: &Value) -> Result<ApplyReport> {
        let hours = keep_since_hours(settings);
        if !apply {
            let images = list_images();
            let count = images.len() as u64;
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: count,
                errors: vec![format!(
                    "dry-run: would prune podman images inactive >{hours}h"
                )],
            });
        }
        let removed = prune_image_count(hours);
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes: 0,
            skipped: 0,
            errors: if removed == 0 {
                vec![]
            } else {
                vec![format!("pruned {removed} images; freed bytes not measured")]
            },
        })
    }
}
