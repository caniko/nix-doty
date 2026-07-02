use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};
use anyhow::Result;

struct NixStoreGcFramework;

impl Framework for NixStoreGcFramework {
    fn name(&self) -> &'static str {
        "nix-store-gc"
    }
    fn summary(&self) -> &'static str {
        "Nix store garbage collection and optimisation"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&NhClean, &NixCollectGarbage, &Optimise]
    }
}

static FRAMEWORK: NixStoreGcFramework = NixStoreGcFramework;

pub static NIX_STORE_GC: &dyn Framework = &FRAMEWORK;

struct NhClean;
impl Variant for NhClean {
    fn name(&self) -> &'static str {
        "nh-clean"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        let store_entries = exec::run_stdout(&["ls", "-1", "/nix/store"])
            .ok()
            .map(|s| s.lines().count() as u64)
            .unwrap_or(0);
        let notes = if exec::path_exists("/run/current-system") {
            "nh clean all -K 14d would prune old generations".into()
        } else {
            "no active NixOS system found".into()
        };
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/nix/store".into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: store_entries,
            notes,
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
            let entries = exec::run_stdout(&["ls", "-1", "/nix/store"])
                .ok()
                .map(|s| s.lines().count() as u64)
                .unwrap_or(0);
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: entries,
                errors: vec!["dry-run: would run nh clean all -K 14d".into()],
            });
        }
        exec::run_stdout(&["nh", "clean", "all", "-K", "14d"])?;
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 1,
            freed_bytes: 0,
            skipped: 0,
            errors: vec!["freed bytes not measured; avoided full /nix/store scan".into()],
        })
    }
}

struct NixCollectGarbage;
impl Variant for NixCollectGarbage {
    fn name(&self) -> &'static str {
        "nix-collect-garbage"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/nix/store".into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: 0,
            notes: "runs sudo nix-collect-garbage -d to delete old generations".into(),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 1,
                errors: vec!["dry-run: would run nix-collect-garbage -d".into()],
            });
        }
        exec::run_stdout(&["sudo", "nix-collect-garbage", "-d"])?;
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 1,
            freed_bytes: 0,
            skipped: 0,
            errors: vec!["freed bytes not measured; avoided full /nix/store scan".into()],
        })
    }
}

struct Optimise;
impl Variant for Optimise {
    fn name(&self) -> &'static str {
        "optimise"
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        Tier::Safe
    }
    fn inspect(&self) -> Result<Inspection> {
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/nix/store".into(),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: 0,
            notes: "hardlinks identical store paths — reduces disk usage".into(),
        })
    }
    fn apply(&self, apply: bool, _force: bool) -> Result<ApplyReport> {
        if !apply {
            return Ok(ApplyReport {
                framework: self.framework().name(),
                variant: self.name(),
                removed: 0,
                freed_bytes: 0,
                skipped: 1,
                errors: vec!["dry-run: would run nix store optimise".into()],
            });
        }
        exec::run_stdout(&["nix", "store", "optimise"])?;
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
