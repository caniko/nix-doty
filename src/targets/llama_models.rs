use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeSet;

const GGUF_DIR: &str = "/data/scratch/models/gguf";

struct LlamaModelsFramework;

impl Framework for LlamaModelsFramework {
    fn name(&self) -> &'static str {
        "llama-models"
    }
    fn summary(&self) -> &'static str {
        "Llama-swap GGUF model cache"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&PruneUnpinned, &DiskReport]
    }
}

static FRAMEWORK: LlamaModelsFramework = LlamaModelsFramework;

pub static LLAMA_MODELS: &dyn Framework = &FRAMEWORK;

struct PruneUnpinned;
impl Variant for PruneUnpinned {
    fn name(&self) -> &'static str {
        "prune-unpinned"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Risky
    }
    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&Value::Object(Default::default()))
    }
    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        let policy = LlamaPolicy::from_settings(settings);
        let unpinned = policy.unpinned_files();
        let count = unpinned.len() as u64;
        let total_size: u64 = unpinned
            .iter()
            .filter_map(|f| exec::file_size(f).ok())
            .sum();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: policy.models_dir,
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: count,
            notes: format!(
                "{count} unpinned gguf files ({:.1} GiB) — risky: deletes only files absent from pinnedFiles",
                total_size as f64 / 1024.0 / 1024.0 / 1024.0
            ),
        })
    }
    fn apply(&self, dry_run: bool, force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(dry_run, force, &Value::Object(Default::default()))
    }
    fn apply_with_settings(
        &self,
        dry_run: bool,
        force: bool,
        settings: &Value,
    ) -> Result<ApplyReport> {
        let policy = LlamaPolicy::from_settings(settings);
        let unpinned = policy.unpinned_files();
        if dry_run {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: unpinned.len() as u64,
                errors: vec!["dry-run: would remove unpinned GGUF files".into()],
            });
        }
        if !force {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 0,
                errors: vec!["requires --force".into()],
            });
        }
        let mut removed = 0u64;
        let mut freed = 0u64;
        for f in &unpinned {
            if let Ok(sz) = exec::file_size(f) {
                freed += sz;
            }
            exec::remove_file(f)?;
            removed += 1;
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
        self.inspect_with_settings(&Value::Object(Default::default()))
    }
    fn inspect_with_settings(&self, settings: &Value) -> Result<Inspection> {
        let policy = LlamaPolicy::from_settings(settings);
        let ggufs = policy.all_files();
        let count = ggufs.len() as u64;
        let total_size: u64 = ggufs.iter().filter_map(|f| exec::file_size(f).ok()).sum();
        let unpinned = policy.unpinned_files().len();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: policy.models_dir,
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: 0,
            notes: format!(
                "{count} gguf files, {unpinned} unpinned, {:.1} GiB total",
                total_size as f64 / 1024.0 / 1024.0 / 1024.0
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

struct LlamaPolicy {
    models_dir: String,
    pinned_files: BTreeSet<String>,
}

impl LlamaPolicy {
    fn from_settings(settings: &Value) -> Self {
        let models_dir = settings
            .get("modelsDir")
            .and_then(Value::as_str)
            .unwrap_or(GGUF_DIR)
            .to_string();
        let pinned_files = settings
            .get("pinnedFiles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|file| {
                if file.starts_with('/') {
                    file.to_string()
                } else {
                    format!("{}/{}", models_dir.trim_end_matches('/'), file)
                }
            })
            .collect();
        Self {
            models_dir,
            pinned_files,
        }
    }

    fn all_files(&self) -> Vec<String> {
        exec::read_dir(&self.models_dir)
            .unwrap_or_default()
            .into_iter()
            .filter(|f| f.ends_with(".gguf"))
            .collect()
    }

    fn unpinned_files(&self) -> Vec<String> {
        self.all_files()
            .into_iter()
            .filter(|file| !self.pinned_files.contains(file))
            .collect()
    }
}
