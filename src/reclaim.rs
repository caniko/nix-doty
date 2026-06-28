use anyhow::{Context, Result};
use serde::Serialize;

use crate::framework::{ApplyReport, Inspection, Tier};
use crate::mount::{self, MountInfo};
use crate::registry;

#[derive(Debug, Clone, Serialize)]
pub struct ReclaimTarget {
    pub framework: &'static str,
    pub variant: &'static str,
    pub tier: &'static str,
    pub mount: String,
    pub estimated_freed_bytes: u64,
    pub actual_freed_bytes: Option<u64>,
    pub status: &'static str,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReclaimPlan {
    pub schema_version: u32,
    pub mount: String,
    pub device: String,
    pub fstype: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub usage_pct: f64,
    pub target_free_bytes: u64,
    pub targets: Vec<ReclaimTarget>,
    pub total_estimated_freed_bytes: u64,
    pub total_actual_freed_bytes: Option<u64>,
    pub final_available_bytes: Option<u64>,
    pub goal_met: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ReclaimConfig {
    pub mount: Option<String>,
    pub threshold_pct: f64,
    pub min_free_bytes: Option<u64>,
    pub min_free_pct: Option<f64>,
    pub apply: bool,
    pub force: bool,
    pub json: bool,
    pub all: bool,
}

impl Default for ReclaimConfig {
    fn default() -> Self {
        Self {
            mount: None,
            threshold_pct: 85.0,
            min_free_bytes: None,
            min_free_pct: None,
            apply: false,
            force: false,
            json: false,
            all: false,
        }
    }
}

pub fn plan(config: &ReclaimConfig) -> Result<Vec<ReclaimPlan>> {
    let all_mounts = mount::read_mounts()?;

    let problem_mounts = if let Some(ref m) = config.mount {
        let mi = all_mounts.iter()
            .find(|mnt| mnt.mount_point == *m)
            .cloned()
            .or_else(|| mount::df(m).ok());
        match mi {
            Some(mi) if mi.usage_pct() >= config.threshold_pct || config.all => vec![mi],
            _ => vec![],
        }
    } else if config.all {
        all_mounts.clone()
    } else {
        all_mounts.iter()
            .filter(|m| m.usage_pct() >= config.threshold_pct)
            .cloned()
            .collect()
    };

    if problem_mounts.is_empty() {
        return Ok(Vec::new());
    }

    let same_device_mounts = same_device_group(&problem_mounts, &all_mounts);

    let mut plans = Vec::new();
    for mount_info in &problem_mounts {
        let target_free = compute_target_free(mount_info, config);
        let targets = find_targets_for_mount_group(mount_info, &same_device_mounts, config)?;
        let total_estimated: u64 = targets.iter().map(|t| t.estimated_freed_bytes).sum();

        plans.push(ReclaimPlan {
            schema_version: 1,
            mount: mount_info.mount_point.clone(),
            device: mount_info.device.clone(),
            fstype: mount_info.fstype.clone(),
            total_bytes: mount_info.total_bytes,
            used_bytes: mount_info.used_bytes,
            available_bytes: mount_info.available_bytes,
            usage_pct: mount_info.usage_pct(),
            target_free_bytes: target_free,
            targets,
            total_estimated_freed_bytes: total_estimated,
            total_actual_freed_bytes: None,
            final_available_bytes: None,
            goal_met: None,
        });
    }

    Ok(plans)
}

fn same_device_group(problem: &[MountInfo], all: &[MountInfo]) -> Vec<MountInfo> {
    let device_ids: std::collections::HashSet<&str> = problem.iter().map(|m| m.device.as_str()).collect();
    all.iter()
        .filter(|m| device_ids.contains(m.device.as_str()))
        .cloned()
        .collect()
}

pub fn execute(plan: &mut ReclaimPlan, config: &ReclaimConfig) -> Result<()> {
    if !config.apply {
        return Ok(());
    }

    let mut total_freed: u64 = 0;

    for target in &mut plan.targets {
        if target.status != "pending" {
            continue;
        }

        let variant = match registry::find_variant(target.framework, target.variant) {
            Some(v) => v,
            None => {
                target.status = "error";
                target.notes = "variant not found in registry".to_string();
                continue;
            }
        };

        let tier = variant.tier();
        if !config.force {
            match tier {
                Tier::Confirm => {
                    target.status = "skipped";
                    target.notes = "confirm — requires --force".to_string();
                    continue;
                }
                Tier::Risky => {
                    target.status = "skipped";
                    target.notes = "risky — requires --force".to_string();
                    continue;
                }
                Tier::ReportOnly => {
                    target.status = "skipped";
                    target.notes = "report-only target".to_string();
                    continue;
                }
                _ => {}
            }
        }

        match variant.apply(false, config.force) {
            Ok(report) => {
                target.actual_freed_bytes = Some(report.freed_bytes);
                target.status = if report.errors.is_empty() {
                    "completed"
                } else {
                    "completed-with-errors"
                };
                if !report.errors.is_empty() {
                    target.notes = report.errors.join("; ");
                }
                total_freed = total_freed.saturating_add(report.freed_bytes);

                let current_available = mount::df(&plan.mount)
                    .map(|m| m.available_bytes)
                    .unwrap_or(0);
                plan.final_available_bytes = Some(current_available);
                plan.total_actual_freed_bytes = Some(total_freed);

                if current_available >= plan.target_free_bytes {
                    plan.goal_met = Some(true);
                    return Ok(());
                }
            }
            Err(e) => {
                target.status = "error";
                target.notes = format!("{e:#}");
            }
        }
    }

    let final_available = mount::df(&plan.mount)
        .map(|m| m.available_bytes)
        .unwrap_or(0);
    plan.final_available_bytes = Some(final_available);
    plan.total_actual_freed_bytes = Some(total_freed);
    plan.goal_met = Some(final_available >= plan.target_free_bytes);

    Ok(())
}

fn compute_target_free(mount: &MountInfo, config: &ReclaimConfig) -> u64 {
    if let Some(bytes) = config.min_free_bytes {
        if bytes > mount.available_bytes {
            return bytes.saturating_sub(mount.available_bytes);
        }
        return 0;
    }

    if let Some(pct) = config.min_free_pct {
        let target = (mount.total_bytes as f64 * pct / 100.0) as u64;
        if target > mount.available_bytes {
            return target.saturating_sub(mount.available_bytes);
        }
        return 0;
    }

    let target = (mount.total_bytes as f64 * 0.10) as u64; // default: 10% free
    if target > mount.available_bytes {
        return target.saturating_sub(mount.available_bytes);
    }
    0
}

fn find_targets_for_mount_group(
    primary_mount: &MountInfo,
    all_same_device: &[MountInfo],
    config: &ReclaimConfig,
) -> Result<Vec<ReclaimTarget>> {
    let mut candidates: Vec<ReclaimTarget> = Vec::new();

    for framework in registry::ALL_FRAMEWORKS {
        for variant in framework.variants() {
            let tier = variant.tier();
            if !config.force {
                match tier {
                    Tier::Confirm | Tier::Risky | Tier::ReportOnly => continue,
                    _ => {}
                }
            }

            let inspection = match variant.inspect() {
                Ok(i) => i,
                Err(_) => continue,
            };

            let target_mount = mount::mount_for_path(&inspection.path)
                .unwrap_or_else(|| "/".to_string());

            let mount_matches = target_mount == primary_mount.mount_point
                || all_same_device.iter().any(|m| m.mount_point == target_mount);

            if !mount_matches {
                continue;
            }

            if inspection.would_remove == 0 && inspection.size_bytes.unwrap_or(0) == 0 {
                continue;
            }

            let estimated = inspection.size_bytes.unwrap_or(0);
            let tier_str = match tier {
                Tier::Safe => "safe",
                Tier::Confirm => "confirm",
                Tier::Risky => "risky",
                Tier::ReportOnly => "report-only",
            };

            candidates.push(ReclaimTarget {
                framework: variant.framework().name(),
                variant: variant.name(),
                tier: tier_str,
                mount: target_mount,
                estimated_freed_bytes: estimated,
                actual_freed_bytes: None,
                status: "pending",
                notes: inspection.notes,
            });
        }
    }

    let tier_order: fn(&Tier) -> u8 = |t| match t {
        Tier::Safe => 0,
        Tier::Confirm => 1,
        Tier::Risky => 2,
        Tier::ReportOnly => 3,
    };

    candidates.sort_by(|a, b| {
        let a_tier_priority = registry::find_variant(a.framework, a.variant)
            .map(|v| tier_order(&v.tier()))
            .unwrap_or(0);
        let b_tier_priority = registry::find_variant(b.framework, b.variant)
            .map(|v| tier_order(&v.tier()))
            .unwrap_or(0);
        a_tier_priority
            .cmp(&b_tier_priority)
            .then(b.estimated_freed_bytes.cmp(&a.estimated_freed_bytes))
    });

    Ok(candidates)
}

pub fn format_plan_human(plans: &[ReclaimPlan]) -> String {
    let mut output = String::new();

    if plans.is_empty() {
        output.push_str("No mounts exceed the threshold. Nothing to reclaim.\n");
        return output;
    }

    for plan in plans {
        output.push_str(&format!(
            "Mount: {} ({}) [{:.1}% full]\n",
            plan.mount, plan.device, plan.usage_pct
        ));
        output.push_str(&format!(
            "  Size: {}  Used: {}  Available: {}  Target: {}\n",
            human_size(plan.total_bytes),
            human_size(plan.used_bytes),
            human_size(plan.available_bytes),
            human_size(plan.target_free_bytes),
        ));
        output.push_str(&format!(
            "  Targets: {} total, estimated {} can be freed\n",
            plan.targets.len(),
            human_size(plan.total_estimated_freed_bytes),
        ));
        output.push_str("\n");

        if plan.targets.is_empty() {
            output.push_str("  No applicable targets found.\n");
            continue;
        }

        output.push_str(&format!(
            "  {:<28} {:<6} {:<12} {:<10} {}\n",
            "Target", "Tier", "Est. Yield", "Status", "Notes"
        ));
        output.push_str(&format!("  {}\n", "-".repeat(90)));

        for target in &plan.targets {
            let yield_str = human_size(target.estimated_freed_bytes);
            let status = target.status;
            output.push_str(&format!(
                "  {:<28} {:<6} {:<12} {:<10} {}\n",
                format!("{}/{}", target.framework, target.variant),
                target.tier,
                yield_str,
                status,
                target.notes,
            ));
        }

        if let Some(actual) = plan.total_actual_freed_bytes {
            let goal = if plan.goal_met.unwrap_or(false) {
                "MET"
            } else {
                "NOT MET"
            };
            output.push_str(&format!(
                "\n  Result: {} freed (goal: {goal})\n",
                human_size(actual),
            ));
            if let Some(final_avail) = plan.final_available_bytes {
                output.push_str(&format!(
                    "  Final available: {}\n",
                    human_size(final_avail),
                ));
            }
        }

        output.push_str("\n");
    }

    output
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
