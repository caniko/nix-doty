use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::config::{self, ConfiguredTarget};
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

struct SelectedVariant {
    variant: &'static dyn crate::framework::Variant,
    settings: Value,
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
        println!("{:<28} {:<24} Description", "Framework", "Variants");
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

pub fn status(
    target: Option<String>,
    variant: Option<String>,
    config_path: &str,
    json: bool,
) -> Result<()> {
    let frameworks = select_variants(target.as_deref(), variant.as_deref(), config_path)?;

    let mut inspections = Vec::new();
    for selected in &frameworks {
        let v = selected.variant;
        match v.inspect_with_settings(&selected.settings) {
            Ok(i) => inspections.push(i),
            Err(e) => {
                eprintln!(
                    "Warning: {}/{} inspect failed: {e}",
                    v.framework().name(),
                    v.name()
                );
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&StatusOutput {
                target,
                variant,
                inspections
            })?
        );
    } else {
        let header = format!(
            "{:<28} {:<6} {:<12} {:<10} {:<8} {}",
            "Framework/Variant", "Tier", "Path", "Size", "Remove", "Notes"
        );
        println!("{header}");
        println!("{}", "-".repeat(120));
        for i in &inspections {
            let key = format!("{}/{}", i.framework, i.variant);
            let tier = match frameworks
                .iter()
                .find(|selected| {
                    selected.variant.framework().name() == i.framework
                        && selected.variant.name() == i.variant
                })
                .map(|selected| selected.variant.tier())
            {
                Some(Tier::Safe) => "safe",
                Some(Tier::Confirm) => "confirm",
                Some(Tier::Risky) => "risky",
                Some(Tier::ReportOnly) => "rpt",
                None => "?",
            };
            let size = i.size_bytes.map(human_size).unwrap_or_else(|| "-".into());
            let _age = i
                .age_oldest_days
                .map(|d| format!("{d}d"))
                .unwrap_or_else(|| "-".into());
            let would_remove = if i.would_remove > 0 {
                i.would_remove.to_string()
            } else {
                "-".into()
            };
            println!(
                "{:<28} {:<6} {:<12} {:<10} {:<8} {}",
                key, tier, i.path, size, would_remove, i.notes
            );
        }
    }
    Ok(())
}

pub fn run(
    target: Option<String>,
    variant: Option<String>,
    config_path: &str,
    apply: bool,
    force: bool,
    json: bool,
) -> Result<()> {
    let variants = select_variants(target.as_deref(), variant.as_deref(), config_path)?;

    let mut reports = Vec::new();
    for selected in &variants {
        let v = selected.variant;
        let tier = v.tier();
        if !force {
            match tier {
                Tier::Risky if apply => {
                    reports.push(skipped_report(v, "skipped: risky, requires --force"));
                    continue;
                }
                Tier::Confirm if apply => {
                    reports.push(skipped_report(v, "skipped: confirm, requires --force"));
                    continue;
                }
                Tier::ReportOnly if apply => {
                    reports.push(skipped_report(v, "skipped: report-only"));
                    continue;
                }
                _ => {}
            }
        }
        match v.apply_with_settings(apply, force, &selected.settings) {
            Ok(r) => {
                if !json {
                    let prefix = if apply { "APPLIED" } else { "DRY-RUN" };
                    let status = apply_status(&r, apply);
                    let details = if !r.errors.is_empty() {
                        if apply {
                            format!("; errors: {}", r.errors.join("; "))
                        } else {
                            let info: Vec<&str> = r
                                .errors
                                .iter()
                                .map(|s| s.as_str())
                                .filter(|e| !e.starts_with("dry-run:"))
                                .collect();
                            if info.is_empty() {
                                String::new()
                            } else {
                                format!("; {}", info.join("; "))
                            }
                        }
                    } else {
                        String::new()
                    };
                    println!(
                        "[{prefix}] {}/{}: {} ({status}){details}",
                        r.framework,
                        r.variant,
                        summary_line(&r, apply),
                    );
                }
                reports.push(r);
            }
            Err(e) => {
                eprintln!(
                    "[{}] {}/{}: command failed ({})",
                    prefix(apply),
                    v.framework().name(),
                    v.name(),
                    e
                );
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&RunOutput {
                target,
                variant,
                reports,
                apply,
                force
            })?
        );
    }
    Ok(())
}

fn prefix(apply: bool) -> &'static str {
    if apply { "APPLIED" } else { "DRY-RUN" }
}

fn apply_status(report: &ApplyReport, apply: bool) -> &'static str {
    if !apply {
        return "dry-run";
    }
    if report.skipped > 0 {
        return "skipped";
    }
    if report.removed > 0 {
        return "applied";
    }
    if !report.errors.is_empty() {
        return "failed";
    }
    "no-op"
}

fn summary_line(report: &ApplyReport, apply: bool) -> String {
    if !apply {
        let would = report.skipped;
        let bytes = human_size(report.freed_bytes);
        if would > 0 {
            format!("would remove {would} items ({bytes})")
        } else {
            "nothing to clean".to_string()
        }
    } else if report.removed > 0 {
        let freed = human_size(report.freed_bytes);
        format!("removed {} items, freed {freed}", report.removed)
    } else if report.skipped > 0 {
        format!("skipped {} items", report.skipped)
    } else if !report.errors.is_empty() {
        let first = &report.errors[0];
        if first.len() > 80 {
            format!("{}...", &first[..77])
        } else {
            first.to_string()
        }
    } else {
        "nothing to clean".to_string()
    }
}

fn skipped_report(v: &dyn crate::framework::Variant, reason: &str) -> ApplyReport {
    eprintln!("[SKIPPED] {}/{}: {reason}", v.framework().name(), v.name());
    ApplyReport {
        framework: v.framework().name(),
        variant: v.name(),
        removed: 0,
        freed_bytes: 0,
        skipped: 1,
        errors: vec![reason.into()],
    }
}

pub fn doctor(config_path: &str, json: bool) -> Result<()> {
    let configured_targets = config::load_targets(config_path)?;
    let configured: Vec<DoctorTarget> = configured_targets
        .iter()
        .map(|t| DoctorTarget {
            name: t.name.clone(),
            variant: t.variant.clone(),
        })
        .collect();

    let mut missing = Vec::new();
    let mut extra = Vec::new();
    let available: BTreeMap<&str, BTreeSet<&str>> = registry::ALL_FRAMEWORKS
        .iter()
        .map(|f| (f.name(), f.variants().iter().map(|v| v.name()).collect()))
        .collect();
    let configured_pairs: BTreeSet<(String, String)> = configured
        .iter()
        .map(|t| (t.name.clone(), t.variant.clone()))
        .collect();

    for t in &configured {
        match available.get(t.name.as_str()) {
            Some(variants) if variants.contains(t.variant.as_str()) => {}
            Some(_) => missing.push(format!("{}/{}", t.name, t.variant)),
            None => missing.push(t.name.clone()),
        }
    }
    for (name, variants) in &available {
        for variant in variants {
            let pair = ((*name).to_string(), (*variant).to_string());
            if !configured_pairs.contains(&pair) {
                extra.push(format!("{name}/{variant}"));
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&DoctorOutput {
                config_path: config_path.into(),
                configured,
                missing,
                extra,
            })?
        );
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

fn select_variants(
    target: Option<&str>,
    variant: Option<&str>,
    config_path: &str,
) -> Result<Vec<SelectedVariant>> {
    match (target, variant) {
        (Some(t), Some(v)) => {
            let configured = configured_for(Some(t), Some(v), config_path)?;
            if configured.is_empty() {
                anyhow::bail!("target {t} variant {v} is not configured in {config_path}");
            }
            configured_to_variants(configured)
        }
        (Some(t), None) => {
            let configured = configured_for(Some(t), None, config_path)?;
            if configured.is_empty() {
                anyhow::bail!("target {t} is not configured in {config_path}");
            }
            configured_to_variants(configured)
        }
        (None, Some(v)) => {
            anyhow::bail!("variant {v} requires --target so configured host policy is unambiguous")
        }
        (None, None) => configured_to_variants(config::load_targets(config_path)?),
    }
}

fn configured_for(
    target: Option<&str>,
    variant: Option<&str>,
    config_path: &str,
) -> Result<Vec<ConfiguredTarget>> {
    let configured = config::load_targets(config_path)?;
    Ok(configured
        .into_iter()
        .filter(|t| target.is_none_or(|name| t.name == name))
        .filter(|t| variant.is_none_or(|name| t.variant == name))
        .collect())
}

fn configured_to_variants(configured: Vec<ConfiguredTarget>) -> Result<Vec<SelectedVariant>> {
    configured
        .into_iter()
        .map(|t| {
            let variant = registry::find_variant(&t.name, &t.variant).ok_or_else(|| {
                anyhow::anyhow!("target {} variant {} not found", t.name, t.variant)
            })?;
            Ok(SelectedVariant {
                variant,
                settings: t.settings,
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
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

    let mut report = crate::reclaim::plan(&config)?;

    if report.filesystems.is_empty() {
        println!(
            "No filesystems exceed the threshold ({}%). Nothing to reclaim.",
            threshold
        );
        return Ok(());
    }

    if apply {
        crate::reclaim::execute(&mut report, &config)?;
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", crate::reclaim::format_report_human(&report));
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
