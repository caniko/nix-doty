use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

struct DnsRuntimeCacheFramework;

impl Framework for DnsRuntimeCacheFramework {
    fn name(&self) -> &'static str {
        "dns-runtime-cache"
    }
    fn summary(&self) -> &'static str {
        "DNS decrypt runtime cache (tmpfs)"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&DnsCacheClear]
    }
}

static FRAMEWORK: DnsRuntimeCacheFramework = DnsRuntimeCacheFramework;

pub static DNS_RUNTIME_CACHE: &dyn Framework = &FRAMEWORK;

fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string())
}

struct DnsCacheClear;
impl Variant for DnsCacheClear {
    fn name(&self) -> &'static str {
        "clear"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let dir = format!("{}/canix-dns", runtime_dir());
        let (count, size) = if exec::path_exists(&dir) {
            let size = exec::total_dir_size(&dir).unwrap_or(0);
            let files = exec::read_dir(&dir).unwrap_or_default().len() as u64;
            (files, size)
        } else {
            (0, 0)
        };
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: dir,
            size_bytes: Some(size),
            age_oldest_days: None,
            would_remove: count,
            notes: if count > 0 {
                format!("{count} cached entries")
            } else {
                "no cache".into()
            },
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        let dir = format!("{}/canix-dns", runtime_dir());
        if !apply {
            let count = exec::dir_entry_count(&dir).unwrap_or(0);
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: count,
                errors: vec![format!("dry-run: would remove {dir}")],
            });
        }
        if exec::path_exists(&dir) {
            exec::remove_dir_all(&dir)?;
        }
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 1,
            freed_bytes: 0,
            skipped: 0,
            errors: vec![],
        })
    }
}
