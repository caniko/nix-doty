use anyhow::Result;
use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};

const GGUF_DIR: &str = "/data/scratch/models/gguf";

struct LlamaModelsFramework;

impl Framework for LlamaModelsFramework {
    fn name(&self) -> &'static str { "llama-models" }
    fn summary(&self) -> &'static str { "Llama-swap GGUF model cache" }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&PruneUnpinned, &DiskReport]
    }
}

static FRAMEWORK: LlamaModelsFramework = LlamaModelsFramework;

pub static LLAMA_MODELS: &dyn Framework = &FRAMEWORK;

struct PruneUnpinned;
impl Variant for PruneUnpinned {
    fn name(&self) -> &'static str { "prune-unpinned" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::Risky }
    fn inspect(&self) -> Result<Inspection> {
        let gguf_files = exec::read_dir(GGUF_DIR).unwrap_or_default();
        let ggufs: Vec<String> = gguf_files.into_iter()
            .filter(|f| f.ends_with(".gguf"))
            .collect();
        let count = ggufs.len() as u64;
        let total_size: u64 = ggufs.iter().filter_map(|f| exec::file_size(f).ok()).sum();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: GGUF_DIR.into(),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: count,
            notes: format!("{count} gguf files ({:.1} GiB) — risky: deletes unpinned models", total_size as f64 / 1024.0 / 1024.0 / 1024.0),
        })
    }
    fn apply(&self, dry_run: bool, force: bool) -> Result<ApplyReport> {
        if dry_run {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0, freed_bytes: 0, skipped: 1,
                errors: vec!["dry-run: would remove unpinned GGUF files".into()],
            });
        }
        if !force {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0, freed_bytes: 0, skipped: 0,
                errors: vec!["requires --force".into()],
            });
        }
        let gguf_files = exec::read_dir(GGUF_DIR).unwrap_or_default();
        let ggufs: Vec<String> = gguf_files.into_iter()
            .filter(|f| f.ends_with(".gguf"))
            .collect();
        let mut removed = 0u64;
        let mut freed = 0u64;
        for f in &ggufs {
            if let Ok(sz) = exec::file_size(f) {
                freed += sz;
            }
            exec::remove_file(f)?;
            removed += 1;
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed, freed_bytes: freed, skipped: 0, errors: vec![],
        })
    }
}

struct DiskReport;
impl Variant for DiskReport {
    fn name(&self) -> &'static str { "disk-report" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::ReportOnly }
    fn inspect(&self) -> Result<Inspection> {
        let gguf_files = exec::read_dir(GGUF_DIR).unwrap_or_default();
        let ggufs: Vec<String> = gguf_files.into_iter()
            .filter(|f| f.ends_with(".gguf"))
            .collect();
        let count = ggufs.len() as u64;
        let total_size: u64 = ggufs.iter().filter_map(|f| exec::file_size(f).ok()).sum();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: GGUF_DIR.into(),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: 0,
            notes: format!("{count} gguf files, {:.1} GiB total", total_size as f64 / 1024.0 / 1024.0 / 1024.0),
        })
    }
    fn apply(&self, _dry_run: bool, _force: bool) -> Result<ApplyReport> {
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 0, freed_bytes: 0, skipped: 0, errors: vec![],
        })
    }
}
