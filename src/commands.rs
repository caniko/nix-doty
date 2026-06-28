use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

use crate::framework::{ApplyReport, Inspection, Tier};
use crate::registry;

#[derive(Serialize)]
struct ListEntry {
    framework: &'static str,
    summary: &'static str,
    variants: Vec<VariantEntry>,
}

#[derive(Serialize)]
struct VariantEntry {
    name: &'static str,
    tier: &'static str,
}

#[derive(Serialize)]
struct StatusOutput {
    target: Option<String>,
    variant: Option<String>,
    inspections: Vec<Inspection>,
}

#[derive(Serialize)]
struct RunOutput {
    target: Option<String>,
    variant: Option<String>,
    reports: Vec<ApplyReport>,
    apply: bool,
    force: bool,
}

#[derive(Serialize)]
struct DoctorOutput {
    config_path: String,
    configured: Vec<DoctorTarget>,
    missing: Vec<String>,
    extra: Vec<String>,
}

#[derive(Serialize)]
struct DoctorTarget {
    name: String,
    variant: String,
}

pub fn list(json: bool) -> Result<()> {
    let entries: Vec<ListEntry> = registry::ALL_FRAMEWORKS
        .iter()
        .map(|f| ListEntry {
            framework: f.name(),
            summary: f.summary(),
            variants: f
                .variants()
                .iter()
                .map(|v| VariantEntry {
                    name: v.name(),
                    tier: match v.tier() {
                        Tier::Safe => "safe",
                        Tier::Confirm => "confirm",
                        Tier::Risky => "risky",
                        Tier::ReportOnly => "report-only",
                    },
                })
                .collect(),
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        println!("{:<28} {:<24} {}", "Framework", "Variants", "Description");
        println!("{}", "-".repeat(80));
        for e in &entries {
            let variant_str = e
                .variants
                .iter()
                .map(|v| format!("{}[{}]", v.name, v.tier))
                .collect::<Vec<_>>()
                .join(", ");
            println!("{:<28} {:<24} {}", e.framework, variant_str, e.summary);
        }
    }
    Ok(())
}

pub fn status(target: Option<String>, variant: Option<String>, json: bool) -> Result<()> {
    let frameworks: Vec<&dyn crate::framework::Variant> = match (&target, &variant) {
        (Some(t), Some(v)) => {
            let v = registry::find_variant(t, v)
                .ok_or_else(|| anyhow::anyhow!("target {t} variant {v} not found"))?;
            vec![v]
        }
        (Some(t), None) => {
            let f = registry::find_framework(t)
                .ok_or_else(|| anyhow::anyhow!("target {t} not found"))?;
            f.variants().iter().copied().collect()
        }
        (None, _) => {
            let mut all: Vec<&dyn crate::framework::Variant> = Vec::new();
            for f in registry::ALL_FRAMEWORKS {
                for v in f.variants() {
                    all.push(*v);
                }
            }
            all
        }
    };

    let mut inspections = Vec::new();
    for v in &frameworks {
        match v.inspect() {
            Ok(i) => inspections.push(i),
            Err(e) => {
                eprintln!("Warning: {}/{} inspect failed: {e}", v.framework().name(), v.name());
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&StatusOutput { target, variant, inspections })?);
    } else {
        let header = format!("{:<28} {:<6} {:<12} {:<10} {:<8} {}", "Framework/Variant", "Tier", "Path", "Size", "Remove", "Notes");
        println!("{header}");
        println!("{}", "-".repeat(120));
        for i in &inspections {
            let key = format!("{}/{}", i.framework, i.variant);
            let tier = match frameworks.iter().find(|v| v.name() == i.variant).map(|v| v.tier()) {
                Some(Tier::Safe) => "safe",
                Some(Tier::Confirm) => "confirm",
                Some(Tier::Risky) => "risky",
                Some(Tier::ReportOnly) => "rpt",
                None => "?",
            };
            let size = i.size_bytes.map(|b| human_size(b)).unwrap_or_else(|| "-".into());
            let _age = i.age_oldest_days.map(|d| format!("{d}d")).unwrap_or_else(|| "-".into());
            let would_remove = if i.would_remove > 0 { i.would_remove.to_string() } else { "-".into() };
            println!("{:<28} {:<6} {:<12} {:<10} {:<8} {}", key, tier, i.path, size, would_remove, i.notes);
        }
    }
    Ok(())
}

pub fn run(
    target: Option<String>,
    variant: Option<String>,
    apply: bool,
    force: bool,
    json: bool,
) -> Result<()> {
    let variants: Vec<&dyn crate::framework::Variant> = match (&target, &variant) {
        (Some(t), Some(v)) => {
            let v = registry::find_variant(t, v)
                .ok_or_else(|| anyhow::anyhow!("target {t} variant {v} not found"))?;
            vec![v]
        }
        (Some(t), None) => {
            let f = registry::find_framework(t)
                .ok_or_else(|| anyhow::anyhow!("target {t} not found"))?;
            f.variants().iter().copied().collect()
        }
        (None, _) => {
            let mut all: Vec<&dyn crate::framework::Variant> = Vec::new();
            for f in registry::ALL_FRAMEWORKS {
                for v in f.variants() {
                    all.push(*v);
                }
            }
            all
        }
    };

    let mut reports = Vec::new();
    for v in &variants {
        let tier = v.tier();
        if !force {
            match tier {
                Tier::Risky if apply => {
                    eprintln!("Skipping {}/{}: risky — use --force to bypass", v.framework().name(), v.name());
                    reports.push(ApplyReport {
                        framework: v.framework().name(),
                        variant: v.name(),
                        removed: 0,
                        freed_bytes: 0,
                        skipped: 1,
                        errors: vec!["skipped: risky, requires --force".into()],
                    });
                    continue;
                }
                Tier::ReportOnly if apply => {
                    eprintln!("Skipping {}/{}: report-only target", v.framework().name(), v.name());
                    reports.push(ApplyReport {
                        framework: v.framework().name(),
                        variant: v.name(),
                        removed: 0,
                        freed_bytes: 0,
                        skipped: 1,
                        errors: vec!["skipped: report-only".into()],
                    });
                    continue;
                }
                _ => {}
            }
        }
        match v.apply(apply, force) {
            Ok(r) => {
                if !json {
                    let prefix = if apply { "APPLIED" } else { "DRY-RUN" };
                    let freed = human_size(r.freed_bytes);
                    println!("[{prefix}] {}/{}: removed {} items, freed {freed}, skipped {}",
                        r.framework, r.variant, r.removed, r.skipped);
                    for err in &r.errors {
                        eprintln!("  error: {err}");
                    }
                }
                reports.push(r);
            }
            Err(e) => {
                eprintln!("Error cleaning {}/{}: {e}", v.framework().name(), v.name());
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&RunOutput { target, variant, reports, apply, force })?);
    }
    Ok(())
}

pub fn doctor(config_path: &str, json: bool) -> Result<()> {
    let config_content = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("cannot read {config_path}: {e}"))?;
    let parsed: serde_json::Value = serde_json::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("cannot parse {config_path}: {e}"))?;
    let configured: Vec<DoctorTarget> = parsed
        .get("targets")
        .map(|v| {
            if let Some(arr) = v.as_array() {
                arr.iter().map(|t| DoctorTarget {
                    name: t["name"].as_str().unwrap_or("?").to_string(),
                    variant: t["variant"].as_str().unwrap_or("?").to_string(),
                }).collect()
            } else if let Some(obj) = v.as_object() {
                obj.iter().map(|(name, t)| DoctorTarget {
                    name: name.clone(),
                    variant: t["variant"].as_str().unwrap_or("?").to_string(),
                }).collect()
            } else {
                Vec::new()
            }
        })
        .unwrap_or_default();

    let mut missing = Vec::new();
    let mut extra = Vec::new();
    let available: BTreeMap<&str, Vec<&str>> = registry::ALL_FRAMEWORKS
        .iter()
        .map(|f| (f.name(), f.variants().iter().map(|v| v.name()).collect()))
        .collect();

    for t in &configured {
        if !available.contains_key(t.name.as_str()) {
            missing.push(t.name.clone());
        }
    }
    for name in available.keys() {
        if !configured.iter().any(|t| t.name == *name) {
            extra.push((*name).to_string());
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&DoctorOutput {
            config_path: config_path.into(),
            configured,
            missing,
            extra,
        })?);
    } else {
        println!("Configured targets (from {config_path}):");
        for t in &configured {
            println!("  ✅ {}/{}", t.name, t.variant);
        }
        if !missing.is_empty() {
            println!("\nMissing from registry:");
            for m in &missing {
                println!("  ❌ {m}");
            }
        }
        if !extra.is_empty() {
            println!("\nAvailable but not configured:");
            for e in &extra {
                println!("  📋 {e}");
            }
        }
    }
    Ok(())
}

pub fn reclaim(
    mount: Option<String>,
    threshold: f64,
    min_free_bytes: Option<u64>,
    min_free_pct: Option<f64>,
    apply: bool,
    force: bool,
    all: bool,
    json: bool,
) -> Result<()> {
    let config = crate::reclaim::ReclaimConfig {
        mount,
        threshold_pct: threshold,
        min_free_bytes,
        min_free_pct,
        apply,
        force,
        json,
        all,
    };

    let mut plans = crate::reclaim::plan(&config)?;

    if plans.is_empty() {
        println!("No mounts exceed the threshold ({}%). Nothing to reclaim.", threshold);
        return Ok(());
    }

    if apply {
        for plan in &mut plans {
            crate::reclaim::execute(plan, &config)?;
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&plans)?);
    } else {
        print!("{}", crate::reclaim::format_plan_human(&plans));
    }

    Ok(())
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    format!("{:.1} {}", size, UNITS[unit_idx])
}
