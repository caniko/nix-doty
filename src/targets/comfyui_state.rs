use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

const COMFYUI_STATE_DIR: &str = "/data/nvme0/comfyui";
const COMFYUI_MODEL_DIR: &str = "/data/scratch/models/comfyui";

struct ComfyuiStateFramework;

impl Framework for ComfyuiStateFramework {
    fn name(&self) -> &'static str {
        "comfyui-state"
    }
    fn summary(&self) -> &'static str {
        "ComfyUI temporary state and model cache"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&ComfyuiTempPrune, &ComfyuiModelCacheReport]
    }
}

static FRAMEWORK: ComfyuiStateFramework = ComfyuiStateFramework;

pub static COMFYUI_STATE: &dyn Framework = &FRAMEWORK;

struct ComfyuiTempPrune;
impl Variant for ComfyuiTempPrune {
    fn name(&self) -> &'static str {
        "temp-prune"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Confirm
    }
    fn inspect(&self) -> Result<Inspection> {
        let temp_dirs = ["temp", "cache"];
        let mut total_size = 0u64;
        let mut truncated = false;
        for sub in &temp_dirs {
            let path = format!("{COMFYUI_STATE_DIR}/{sub}");
            if exec::path_exists(&path) {
                let scan = exec::total_dir_size_bounded(&path, 20_000).unwrap_or_default();
                total_size += scan.bytes;
                truncated |= scan.truncated;
            }
        }
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: COMFYUI_STATE_DIR.into(),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: total_size,
            notes: format!(
                "{:.1} MiB in temp/cache dirs{}",
                total_size as f64 / 1024.0 / 1024.0,
                if truncated { " (scan truncated)" } else { "" }
            ),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        let dirs = [
            format!("{COMFYUI_STATE_DIR}/temp"),
            format!("{COMFYUI_STATE_DIR}/cache"),
        ];
        let mut freed = 0u64;
        let mut removed = 0u64;
        for d in &dirs {
            if exec::path_exists(d) {
                if !apply {
                    freed += exec::total_dir_size(d).unwrap_or(0);
                    removed += 1;
                } else {
                    freed += exec::total_dir_size(d).unwrap_or(0);
                    let _ = exec::remove_dir_all(d);
                    removed += 1;
                }
            }
        }
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: removed,
                errors: vec![format!(
                    "dry-run: would free {:.1} MiB",
                    freed as f64 / 1024.0 / 1024.0
                )],
            });
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed,
            freed_bytes: freed,
            skipped: 0,
            errors: vec![],
        })
    }
}

struct ComfyuiModelCacheReport;
impl Variant for ComfyuiModelCacheReport {
    fn name(&self) -> &'static str {
        "model-cache-report"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::ReportOnly
    }
    fn inspect(&self) -> Result<Inspection> {
        let (size, truncated) = if exec::path_exists(COMFYUI_MODEL_DIR) {
            let scan = exec::total_dir_size_bounded(COMFYUI_MODEL_DIR, 20_000).unwrap_or_default();
            (Some(scan.bytes), scan.truncated)
        } else {
            (None, false)
        };
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: COMFYUI_MODEL_DIR.into(),
            size_bytes: size,
            age_oldest_days: None,
            would_remove: 0,
            notes: format!(
                "model directory — never automatically removed{}",
                if truncated { " (scan truncated)" } else { "" }
            ),
        })
    }
    fn apply(&self, _dry_run: bool, _force: bool) -> Result<ApplyReport> {
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
