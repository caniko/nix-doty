use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use crate::framework::Tier;
use crate::mount::{self, MountInfo};
use crate::registry;

#[derive(Debug, Clone, Serialize)]
pub struct ReclaimReport {
    pub schema_version: u32,
    pub threshold_pct: f64,
    pub apply: bool,
    pub force: bool,
    pub totals: ReclaimTotals,
    pub filesystems: Vec<ReclaimFilesystem>,
    pub unassigned_targets: Vec<ReclaimTarget>,
    pub health: Vec<HealthEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthEntry {
    pub device: String,
    pub mounts: Vec<String>,
    pub usage_pct: f64,
    pub below_threshold: bool,
    pub needed_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReclaimTotals {
    pub filesystems_total: usize,
    pub filesystems_over_threshold: usize,
    pub targets_total: usize,
    pub skipped_targets: usize,
    pub total_estimated_freed_bytes: u64,
    pub skipped_estimated_freed_bytes: u64,
    pub total_target_free_bytes: u64,
    pub total_goal_shortfall_bytes: u64,
    pub total_actual_freed_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReclaimFilesystem {
    pub device: String,
    pub fstype: String,
    pub maj_min: String,
    pub fsroot: Option<String>,
    pub mounts: Vec<String>,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub usage_pct: f64,
    pub target_free_bytes: u64,
    pub estimated_freed_bytes: u64,
    pub actual_freed_bytes: Option<u64>,
    pub final_available_bytes: Option<u64>,
    pub goal_met: Option<bool>,
    pub targets: Vec<ReclaimTarget>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReclaimTarget {
    pub framework: &'static str,
    pub variant: &'static str,
    pub tier: &'static str,
    pub scope: TargetScope,
    pub paths: Vec<String>,
    pub surfaces: Vec<String>,
    pub estimated_freed_bytes: u64,
    pub actual_freed_bytes: Option<u64>,
    pub status: &'static str,
    pub skip_reason: Option<String>,
    pub notes: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetScope {
    SingleSurface,
    MultiSurface,
    Unassigned,
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

#[derive(Debug, Clone)]
struct FilesystemGroup {
    key: String,
    device: String,
    fstype: String,
    mounts: Vec<MountInfo>,
    representative: MountInfo,
}

#[derive(Debug, Clone)]
struct ResolvedTarget {
    target: ReclaimTarget,
    filesystem_keys: Vec<String>,
}

pub fn plan(config: &ReclaimConfig) -> Result<ReclaimReport> {
    let all_mounts = mount::read_mounts()?;
    let groups = filesystem_groups(&all_mounts);
    let selected_keys = selected_filesystem_keys(config, &groups)?;

    if selected_keys.is_empty() {
        return Ok(empty_report(config));
    }

    let selected_key_set: BTreeSet<String> = selected_keys.iter().cloned().collect();
    let mut filesystems: Vec<ReclaimFilesystem> = groups
        .iter()
        .filter(|group| selected_key_set.contains(&group.key))
        .map(|group| filesystem_report(group, config))
        .collect();

    let mut unassigned_targets = Vec::new();
    for resolved in inspect_targets(config, &groups)? {
        match resolved.target.scope {
            TargetScope::SingleSurface => {
                if let Some(key) = resolved.filesystem_keys.first() {
                    if let Some(fs) = filesystems
                        .iter_mut()
                        .find(|fs| fs_key_from_report(fs) == *key)
                    {
                        if resolved.target.status != "skipped" {
                            fs.estimated_freed_bytes = fs
                                .estimated_freed_bytes
                                .saturating_add(resolved.target.estimated_freed_bytes);
                        }
                        fs.targets.push(resolved.target);
                    }
                }
            }
            TargetScope::MultiSurface => {
                let include = if config.mount.is_some() {
                    resolved
                        .filesystem_keys
                        .iter()
                        .all(|key| selected_key_set.contains(key))
                } else {
                    resolved
                        .filesystem_keys
                        .iter()
                        .any(|key| selected_key_set.contains(key))
                };
                if include {
                    unassigned_targets.push(resolved.target);
                }
            }
            TargetScope::Unassigned => {
                if config.mount.is_none() {
                    unassigned_targets.push(resolved.target);
                }
            }
        }
    }

    sort_report_targets(&mut filesystems, &mut unassigned_targets);
    let totals = compute_totals(&filesystems, &unassigned_targets);
    let health = compute_health(&filesystems, config.threshold_pct);

    Ok(ReclaimReport {
        schema_version: 2,
        threshold_pct: config.threshold_pct,
        apply: config.apply,
        force: config.force,
        totals,
        filesystems,
        unassigned_targets,
        health,
    })
}

pub fn execute(report: &mut ReclaimReport, config: &ReclaimConfig) -> Result<()> {
    if !config.apply {
        return Ok(());
    }

    let mut total_freed = 0u64;

    for fs in &mut report.filesystems {
        let mut fs_freed = 0u64;
        for target in &mut fs.targets {
            fs_freed = fs_freed.saturating_add(execute_target(target, config)?);
        }
        fs.actual_freed_bytes = Some(fs_freed);
        total_freed = total_freed.saturating_add(fs_freed);
        refresh_filesystem_result(fs);
    }

    for target in &mut report.unassigned_targets {
        total_freed = total_freed.saturating_add(execute_target(target, config)?);
    }

    report.totals.total_actual_freed_bytes = Some(total_freed);
    report.totals = compute_totals(&report.filesystems, &report.unassigned_targets);
    report.totals.total_actual_freed_bytes = Some(total_freed);

    Ok(())
}

fn execute_target(target: &mut ReclaimTarget, config: &ReclaimConfig) -> Result<u64> {
    if target.status != "pending" {
        return Ok(0);
    }

    let variant = match registry::find_variant(target.framework, target.variant) {
        Some(v) => v,
        None => {
            target.status = "error";
            target.notes = "variant not found in registry".to_string();
            return Ok(0);
        }
    };

    if !config.force {
        match variant.tier() {
            Tier::Confirm => {
                target.status = "skipped";
                target.skip_reason = Some("confirm tier requires --force".to_string());
                return Ok(0);
            }
            Tier::Risky => {
                target.status = "skipped";
                target.skip_reason = Some("risky tier requires --force".to_string());
                return Ok(0);
            }
            Tier::ReportOnly => {
                target.status = "skipped";
                target.skip_reason = Some("report-only target".to_string());
                return Ok(0);
            }
            Tier::Safe => {}
        }
    }

    match variant.apply(true, config.force) {
        Ok(apply_report) => {
            target.actual_freed_bytes = Some(apply_report.freed_bytes);
            target.status = if apply_report.errors.is_empty() {
                "completed"
            } else {
                "completed-with-errors"
            };
            if !apply_report.errors.is_empty() {
                target.notes = apply_report.errors.join("; ");
            }
            Ok(apply_report.freed_bytes)
        }
        Err(e) => {
            target.status = "error";
            target.notes = format!("{e:#}");
            Ok(0)
        }
    }
}

fn empty_report(config: &ReclaimConfig) -> ReclaimReport {
    ReclaimReport {
        schema_version: 2,
        threshold_pct: config.threshold_pct,
        apply: config.apply,
        force: config.force,
        totals: ReclaimTotals::default(),
        filesystems: Vec::new(),
        unassigned_targets: Vec::new(),
        health: Vec::new(),
    }
}

fn filesystem_groups(mounts: &[MountInfo]) -> Vec<FilesystemGroup> {
    let mut by_key: BTreeMap<String, Vec<MountInfo>> = BTreeMap::new();
    for mount in mounts {
        by_key.entry(fs_key(mount)).or_default().push(mount.clone());
    }

    by_key
        .into_iter()
        .map(|(key, mut mounts)| {
            mounts.sort_by(|a, b| {
                a.mount_point
                    .len()
                    .cmp(&b.mount_point.len())
                    .then(a.mount_point.cmp(&b.mount_point))
            });
            let representative = mounts[0].clone();
            FilesystemGroup {
                key,
                device: representative.device.clone(),
                fstype: representative.fstype.clone(),
                mounts,
                representative,
            }
        })
        .collect()
}

fn selected_filesystem_keys(
    config: &ReclaimConfig,
    groups: &[FilesystemGroup],
) -> Result<Vec<String>> {
    if let Some(ref requested_mount) = config.mount {
        let requested_mount = normalize_mount(requested_mount);
        let requested = groups
            .iter()
            .find(|group| {
                group
                    .mounts
                    .iter()
                    .any(|mount| mount.mount_point == requested_mount)
            })
            .map(|group| group.representative.clone())
            .or_else(|| mount::df(&requested_mount).ok());

        let Some(requested) = requested else {
            return Ok(Vec::new());
        };

        if requested.usage_pct() < config.threshold_pct && !config.all {
            return Ok(Vec::new());
        }

        return Ok(vec![fs_key(&requested)]);
    }

    Ok(groups
        .iter()
        .filter(|group| config.all || group.representative.usage_pct() >= config.threshold_pct)
        .map(|group| group.key.clone())
        .collect())
}

fn filesystem_report(group: &FilesystemGroup, config: &ReclaimConfig) -> ReclaimFilesystem {
    let representative = &group.representative;
    ReclaimFilesystem {
        device: group.device.clone(),
        fstype: group.fstype.clone(),
        maj_min: representative.maj_min.clone(),
        fsroot: representative.fsroot.clone(),
        mounts: group
            .mounts
            .iter()
            .map(|mount| mount.mount_point.clone())
            .collect(),
        total_bytes: representative.total_bytes,
        used_bytes: representative.used_bytes,
        available_bytes: representative.available_bytes,
        usage_pct: representative.usage_pct(),
        target_free_bytes: compute_target_free(representative, config),
        estimated_freed_bytes: 0,
        actual_freed_bytes: None,
        final_available_bytes: None,
        goal_met: None,
        targets: Vec::new(),
    }
}

fn inspect_targets(
    config: &ReclaimConfig,
    groups: &[FilesystemGroup],
) -> Result<Vec<ResolvedTarget>> {
    let mut targets = Vec::new();

    for framework in registry::ALL_FRAMEWORKS {
        for variant in framework.variants() {
            let tier = variant.tier();
            let mut skip_reason = None;
            let mut status = "pending";
            if !config.force {
                match tier {
                    Tier::Confirm => {
                        status = "skipped";
                        skip_reason = Some("confirm tier requires --force".to_string());
                    }
                    Tier::Risky => {
                        status = "skipped";
                        skip_reason = Some("risky tier requires --force".to_string());
                    }
                    Tier::ReportOnly => {
                        status = "skipped";
                        skip_reason = Some("report-only target".to_string());
                    }
                    Tier::Safe => {}
                }
            }

            let inspection = match variant.inspect() {
                Ok(i) => i,
                Err(_) => continue,
            };

            if inspection.would_remove == 0 && inspection.size_bytes.unwrap_or(0) == 0 {
                continue;
            }

            let paths = target_paths(&inspection.path);
            let mut filesystem_keys = BTreeSet::new();
            let mut surfaces = BTreeSet::new();
            for path in &paths {
                if let Some((key, surface)) = resolve_path_surface(path, groups) {
                    filesystem_keys.insert(key);
                    surfaces.insert(surface);
                }
            }

            let scope = match filesystem_keys.len() {
                0 => TargetScope::Unassigned,
                1 => TargetScope::SingleSurface,
                _ => TargetScope::MultiSurface,
            };

            let target = ReclaimTarget {
                framework: variant.framework().name(),
                variant: variant.name(),
                tier: tier_label(tier),
                scope,
                paths: if paths.is_empty() {
                    vec![inspection.path.clone()]
                } else {
                    paths
                },
                surfaces: surfaces.into_iter().collect(),
                estimated_freed_bytes: inspection.size_bytes.unwrap_or(0),
                actual_freed_bytes: None,
                status,
                skip_reason,
                notes: inspection.notes,
            };

            targets.push(ResolvedTarget {
                target,
                filesystem_keys: filesystem_keys.into_iter().collect(),
            });
        }
    }

    Ok(targets)
}

fn target_paths(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|path| is_real_absolute_path(path))
        .map(ToOwned::to_owned)
        .collect()
}

fn is_real_absolute_path(path: &str) -> bool {
    path.starts_with('/') && !path.contains('<') && !path.contains('>')
}

fn resolve_path_surface(path: &str, groups: &[FilesystemGroup]) -> Option<(String, String)> {
    let mut best: Option<(&FilesystemGroup, &MountInfo)> = None;
    for group in groups {
        for mount in &group.mounts {
            if path_is_on_mount(path, &mount.mount_point) {
                match best {
                    Some((_, best_mount))
                        if best_mount.mount_point.len() >= mount.mount_point.len() => {}
                    _ => best = Some((group, mount)),
                }
            }
        }
    }

    best.map(|(group, mount)| (group.key.clone(), mount.mount_point.clone()))
}

fn path_is_on_mount(path: &str, mount_point: &str) -> bool {
    if mount_point == "/" {
        return path.starts_with('/');
    }
    path == mount_point
        || path
            .strip_prefix(mount_point)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn compute_target_free(mount: &MountInfo, config: &ReclaimConfig) -> u64 {
    if let Some(bytes) = config.min_free_bytes {
        return bytes.saturating_sub(mount.available_bytes);
    }

    if let Some(pct) = config.min_free_pct {
        let target = (mount.total_bytes as f64 * pct / 100.0) as u64;
        return target.saturating_sub(mount.available_bytes);
    }

    let target = (mount.total_bytes as f64 * 0.10) as u64;
    target.saturating_sub(mount.available_bytes)
}

fn compute_health(filesystems: &[ReclaimFilesystem], threshold_pct: f64) -> Vec<HealthEntry> {
    filesystems
        .iter()
        .map(|fs| {
            let threshold_free = (fs.total_bytes as f64 * (100.0 - threshold_pct) / 100.0) as u64;
            let below_threshold = fs.available_bytes >= threshold_free;
            let needed_bytes = if below_threshold {
                0
            } else {
                threshold_free.saturating_sub(fs.available_bytes)
            };
            HealthEntry {
                device: fs.device.clone(),
                mounts: fs.mounts.clone(),
                usage_pct: fs.usage_pct,
                below_threshold,
                needed_bytes,
            }
        })
        .collect()
}

fn compute_totals(
    filesystems: &[ReclaimFilesystem],
    unassigned_targets: &[ReclaimTarget],
) -> ReclaimTotals {
    let fs_target_count: usize = filesystems.iter().map(|fs| fs.targets.len()).sum();
    let fs_estimated: u64 = filesystems.iter().map(|fs| fs.estimated_freed_bytes).sum();
    let unassigned_estimated: u64 = unassigned_targets
        .iter()
        .filter(|target| target.status != "skipped")
        .map(|target| target.estimated_freed_bytes)
        .sum();
    let skipped_estimated_freed_bytes: u64 = filesystems
        .iter()
        .flat_map(|fs| fs.targets.iter())
        .chain(unassigned_targets.iter())
        .filter(|target| target.status == "skipped")
        .map(|target| target.estimated_freed_bytes)
        .sum();
    let skipped_targets = filesystems
        .iter()
        .flat_map(|fs| fs.targets.iter())
        .chain(unassigned_targets.iter())
        .filter(|target| target.status == "skipped")
        .count();
    let total_target_free_bytes: u64 = filesystems.iter().map(|fs| fs.target_free_bytes).sum();
    let total_goal_shortfall_bytes: u64 = filesystems
        .iter()
        .map(|fs| {
            fs.target_free_bytes
                .saturating_sub(fs.estimated_freed_bytes)
        })
        .sum();

    ReclaimTotals {
        filesystems_total: filesystems.len(),
        filesystems_over_threshold: filesystems.len(),
        targets_total: fs_target_count + unassigned_targets.len(),
        skipped_targets,
        total_estimated_freed_bytes: fs_estimated.saturating_add(unassigned_estimated),
        skipped_estimated_freed_bytes,
        total_target_free_bytes,
        total_goal_shortfall_bytes,
        total_actual_freed_bytes: None,
    }
}

fn refresh_filesystem_result(fs: &mut ReclaimFilesystem) {
    let df = fs.mounts.first().and_then(|mount| mount::df(mount).ok());
    if let Some(df) = df {
        fs.final_available_bytes = Some(df.available_bytes);
        fs.goal_met = Some(df.available_bytes >= fs.target_free_bytes);
    } else {
        fs.final_available_bytes = None;
        fs.goal_met = Some(false);
    }
}

fn sort_report_targets(
    filesystems: &mut [ReclaimFilesystem],
    unassigned_targets: &mut [ReclaimTarget],
) {
    for fs in filesystems {
        let mounts: Vec<&str> = fs.mounts.iter().map(|s| s.as_str()).collect();
        fs.targets
            .sort_by(|a, b| target_sort_with_surfaces(a, b, &mounts));
    }
    unassigned_targets.sort_by(target_sort);
}

fn target_sort_with_surfaces(
    a: &ReclaimTarget,
    b: &ReclaimTarget,
    mounts: &[&str],
) -> std::cmp::Ordering {
    surface_priority(a, mounts)
        .cmp(&surface_priority(b, mounts))
        .then(target_sort(a, b))
}

fn surface_priority(target: &ReclaimTarget, mounts: &[&str]) -> u8 {
    if target.surfaces.is_empty() {
        return 2;
    }
    if target.scope != TargetScope::SingleSurface {
        return 1;
    }
    let matches = target.surfaces.iter().any(|s| mounts.contains(&s.as_str()));
    if matches { 0 } else { 1 }
}

fn target_sort(a: &ReclaimTarget, b: &ReclaimTarget) -> std::cmp::Ordering {
    tier_priority(a.tier)
        .cmp(&tier_priority(b.tier))
        .then(b.estimated_freed_bytes.cmp(&a.estimated_freed_bytes))
        .then(a.framework.cmp(b.framework))
        .then(a.variant.cmp(b.variant))
}

fn tier_priority(tier: &str) -> u8 {
    match tier {
        "safe" => 0,
        "confirm" => 1,
        "risky" => 2,
        "report-only" => 3,
        _ => 4,
    }
}

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Safe => "safe",
        Tier::Confirm => "confirm",
        Tier::Risky => "risky",
        Tier::ReportOnly => "report-only",
    }
}

fn fs_key(mount: &MountInfo) -> String {
    match &mount.fsroot {
        Some(fsroot) => format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            mount.device, mount.maj_min, mount.fstype, fsroot
        ),
        None => format!(
            "{}\u{1f}{}\u{1f}{}",
            mount.device, mount.maj_min, mount.fstype
        ),
    }
}

fn fs_key_from_report(fs: &ReclaimFilesystem) -> String {
    match &fs.fsroot {
        Some(fsroot) => format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            fs.device, fs.maj_min, fs.fstype, fsroot
        ),
        None => format!("{}\u{1f}{}\u{1f}{}", fs.device, fs.maj_min, fs.fstype),
    }
}

fn normalize_mount(mount: &str) -> String {
    let mount = mount.trim_end_matches('/');
    if mount.is_empty() {
        "/".to_string()
    } else {
        mount.to_string()
    }
}

pub fn format_report_human(report: &ReclaimReport) -> String {
    let mut output = String::new();

    if report.filesystems.is_empty() {
        output.push_str(&format!(
            "No filesystems exceed the threshold ({:.1}%). Nothing to reclaim.\n",
            report.threshold_pct
        ));
        return output;
    }

    output.push_str("Reclaim summary\n");
    output.push_str(&format!(
        "  Filesystems: {} over threshold  Targets: {} total ({} skipped by tier policy)\n",
        report.totals.filesystems_over_threshold,
        report.totals.targets_total,
        report.totals.skipped_targets
    ));
    output.push_str(&format!(
        "  Estimated reclaim: {}  Goal: {}  Remaining shortfall: {}\n\n",
        human_size(report.totals.total_estimated_freed_bytes),
        human_size(report.totals.total_target_free_bytes),
        human_size(report.totals.total_goal_shortfall_bytes)
    ));
    if report.totals.skipped_estimated_freed_bytes > 0 {
        output.push_str(&format!(
            "  Gated by tier policy: {} (use --force to include confirm/risky/report-only targets)\n\n",
            human_size(report.totals.skipped_estimated_freed_bytes)
        ));
    }

    for fs in &report.filesystems {
        output.push_str(&format!(
            "Filesystem: {} ({}) [{:.1}% full]\n",
            fs.device, fs.fstype, fs.usage_pct
        ));
        output.push_str(&format!("  Mounts: {}\n", fs.mounts.join(", ")));
        output.push_str(&format!(
            "  Size: {}  Used: {}  Available: {}  Target: {}  Est. reclaim: {}\n",
            human_size(fs.total_bytes),
            human_size(fs.used_bytes),
            human_size(fs.available_bytes),
            human_size(fs.target_free_bytes),
            human_size(fs.estimated_freed_bytes)
        ));

        if fs.targets.is_empty() {
            output.push_str("  No filesystem-local targets found.\n\n");
            continue;
        }

        output.push('\n');
        output.push_str(&format!(
            "  {:<36} {:<11} {:<12} {:<10} {:<18} {}\n",
            "Target", "Tier", "Est. Yield", "Status", "Surface", "Notes"
        ));
        output.push_str(&format!("  {}\n", "-".repeat(112)));
        for target in &fs.targets {
            output.push_str(&format_target_row(target));
        }

        if let Some(actual) = fs.actual_freed_bytes {
            let goal = if fs.goal_met.unwrap_or(false) {
                "MET"
            } else {
                "NOT MET"
            };
            output.push_str(&format!(
                "\n  Result: {} freed (goal: {goal})\n",
                human_size(actual)
            ));
            if let Some(final_avail) = fs.final_available_bytes {
                output.push_str(&format!("  Final available: {}\n", human_size(final_avail)));
            }
        }

        output.push('\n');
    }

    if !report.unassigned_targets.is_empty() {
        output.push_str("Global and unassigned actions\n");
        output.push_str(&format!(
            "  {:<36} {:<11} {:<12} {:<10} {:<18} {}\n",
            "Target", "Tier", "Est. Yield", "Status", "Surface", "Notes"
        ));
        output.push_str(&format!("  {}\n", "-".repeat(112)));
        for target in &report.unassigned_targets {
            output.push_str(&format_target_row(target));
        }
        output.push('\n');
    }

    if !report.health.is_empty() {
        output.push_str("Remaining pressure\n");
        for h in &report.health {
            let mounts = h.mounts.join(", ");
            if h.below_threshold {
                output.push_str(&format!(
                    "  {} ({}): {:.1}% \u{2713} below threshold\n",
                    h.device, mounts, h.usage_pct,
                ));
            } else {
                output.push_str(&format!(
                    "  {} ({}): {:.1}% \u{2014} needs {} to drop below {:.0}%\n",
                    h.device,
                    mounts,
                    h.usage_pct,
                    human_size(h.needed_bytes),
                    report.threshold_pct,
                ));
            }
        }
        output.push('\n');
    }

    output
}

fn format_target_row(target: &ReclaimTarget) -> String {
    let surfaces = if target.surfaces.is_empty() {
        match target.scope {
            TargetScope::SingleSurface => "\u{2014}".to_string(),
            TargetScope::MultiSurface => "multi-surface".to_string(),
            TargetScope::Unassigned => "unassigned".to_string(),
        }
    } else {
        target.surfaces.join(", ")
    };
    let notes = if target.paths.is_empty() {
        target.notes.clone()
    } else {
        format!("{} \u{2014} {}", target.notes, target.paths.join(", "))
    };
    let notes = target
        .skip_reason
        .as_ref()
        .map(|reason| format!("{reason}; {notes}"))
        .unwrap_or(notes);

    format!(
        "  {:<36} {:<11} {:<12} {:<10} {:<18} {}\n",
        format!("{}/{}", target.framework, target.variant),
        target.tier,
        human_size(target.estimated_freed_bytes),
        target.status,
        truncate(&surfaces, 18),
        notes
    )
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        prefix
    } else {
        value.to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(mount_point: &str, device: &str, total: u64, used: u64) -> MountInfo {
        MountInfo {
            mount_point: mount_point.to_string(),
            device: device.to_string(),
            fstype: "btrfs".to_string(),
            maj_min: String::new(),
            fsroot: None,
            total_bytes: total,
            used_bytes: used,
            available_bytes: total - used,
        }
    }

    fn sample_target(
        framework: &'static str,
        variant: &'static str,
        surface: &str,
    ) -> ReclaimTarget {
        ReclaimTarget {
            framework,
            variant,
            tier: "safe",
            scope: TargetScope::SingleSurface,
            paths: vec![format!("{surface}/cache")],
            surfaces: vec![surface.to_string()],
            estimated_freed_bytes: 1024,
            actual_freed_bytes: None,
            status: "pending",
            skip_reason: None,
            notes: "test target".to_string(),
        }
    }

    #[test]
    fn filesystem_groups_collapse_mount_aliases_by_backing_filesystem() {
        let mounts = vec![
            mount("/", "/dev/nvme0n1p2", 100, 90),
            mount("/.snapshots", "/dev/nvme0n1p2", 100, 90),
            mount("/home", "/dev/nvme0n1p2", 100, 90),
            mount("/nix", "/dev/nvme0n1p2", 100, 90),
            mount("/data/nvme0", "/dev/nvme0n1p4", 500, 450),
            mount("/data/nvme0/can/Projects", "/dev/nvme0n1p4", 500, 450),
        ];

        let groups = filesystem_groups(&mounts);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].mounts.len(), 4);
        assert_eq!(groups[1].mounts.len(), 2);
        assert!(groups[0].mounts.iter().any(|m| m.mount_point == "/home"));
        assert!(
            groups[1]
                .mounts
                .iter()
                .any(|m| m.mount_point == "/data/nvme0/can/Projects")
        );
    }

    #[test]
    fn path_resolution_uses_most_specific_mount_and_separates_surfaces() {
        let mounts = vec![
            mount("/", "/dev/nvme0n1p2", 100, 90),
            mount("/home", "/dev/nvme0n1p2", 100, 90),
            mount("/data/nvme0", "/dev/nvme0n1p4", 500, 450),
        ];
        let groups = filesystem_groups(&mounts);

        let home = resolve_path_surface("/home/can/.cache/go-build", &groups).unwrap();
        let data = resolve_path_surface("/data/nvme0/can/Projects/doty", &groups).unwrap();

        assert_ne!(home.0, data.0);
        assert_eq!(home.1, "/home");
        assert_eq!(data.1, "/data/nvme0");
    }

    #[test]
    fn human_format_lists_aliases_once_and_keeps_unassigned_separate() {
        let report = ReclaimReport {
            schema_version: 2,
            threshold_pct: 85.0,
            apply: false,
            force: false,
            totals: ReclaimTotals {
                filesystems_total: 1,
                filesystems_over_threshold: 1,
                targets_total: 2,
                skipped_targets: 0,
                total_estimated_freed_bytes: 2048,
                skipped_estimated_freed_bytes: 0,
                total_target_free_bytes: 0,
                total_goal_shortfall_bytes: 0,
                total_actual_freed_bytes: None,
            },
            filesystems: vec![ReclaimFilesystem {
                device: "/dev/nvme0n1p2".to_string(),
                fstype: "btrfs".to_string(),
                maj_min: String::new(),
                fsroot: None,
                mounts: vec![
                    "/".to_string(),
                    "/home".to_string(),
                    "/nix".to_string(),
                    "/.snapshots".to_string(),
                ],
                total_bytes: 100,
                used_bytes: 90,
                available_bytes: 10,
                usage_pct: 90.0,
                target_free_bytes: 0,
                estimated_freed_bytes: 1024,
                actual_freed_bytes: None,
                final_available_bytes: None,
                goal_met: None,
                targets: vec![sample_target("user-cache", "purge-go-build", "/home")],
            }],
            unassigned_targets: vec![ReclaimTarget {
                framework: "failed-units",
                variant: "reset",
                tier: "safe",
                scope: TargetScope::Unassigned,
                paths: vec!["systemctl --failed".to_string()],
                surfaces: Vec::new(),
                estimated_freed_bytes: 1024,
                actual_freed_bytes: None,
                status: "pending",
                skip_reason: None,
                notes: "system action".to_string(),
            }],
            health: Vec::new(),
        };

        let formatted = format_report_human(&report);

        assert!(formatted.contains("Mounts: /, /home, /nix, /.snapshots"));
        assert_eq!(formatted.matches("user-cache/purge-go-build").count(), 1);
        assert!(formatted.contains("Global and unassigned actions"));
        assert!(formatted.contains("failed-units/reset"));
    }

    #[test]
    fn report_serializes_schema_version_two() {
        let report = empty_report(&ReclaimConfig::default());
        let json = serde_json::to_value(report).unwrap();

        assert_eq!(json["schema_version"], 2);
        assert!(json.get("filesystems").is_some());
        assert!(json.get("unassigned_targets").is_some());
        assert!(json.get("totals").is_some());
        assert!(json.get("health").is_some());
    }
}
