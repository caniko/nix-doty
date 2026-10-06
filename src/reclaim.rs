use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use crate::framework::Tier;
use crate::mount::{self, MountInfo};
use crate::registry;

pub mod runtime;
use runtime::{ActionContext, Limits, Runtime};

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
    pub errors: Vec<String>,
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
    pub unknown_estimates: usize,
    pub net_available_change_bytes: Option<i128>,
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
    pub required_available_bytes: u64,
    pub estimated_freed_bytes: u64,
    pub actual_freed_bytes: Option<u64>,
    pub final_available_bytes: Option<u64>,
    pub goal_met: Option<bool>,
    pub net_available_change_bytes: Option<i128>,
    pub measurement_error: Option<String>,
    pub targets: Vec<ReclaimTarget>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReclaimTarget {
    #[serde(skip)]
    settings: serde_json::Value,
    pub framework: &'static str,
    pub variant: &'static str,
    pub tier: &'static str,
    pub scope: TargetScope,
    pub paths: Vec<String>,
    pub surfaces: Vec<String>,
    pub estimated_freed_bytes: Option<u64>,
    pub actual_freed_bytes: Option<u64>,
    pub status: &'static str,
    pub skip_reason: Option<String>,
    pub notes: String,
    pub command_log: String,
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
    pub config_path: String,
    pub mount: Option<String>,
    pub threshold_pct: f64,
    pub min_free_bytes: Option<u64>,
    pub min_free_pct: Option<f64>,
    pub apply: bool,
    pub force: bool,
    pub json: bool,
    pub all: bool,
    pub limits: Limits,
}

impl Default for ReclaimConfig {
    fn default() -> Self {
        Self {
            config_path: crate::config::DEFAULT_CONFIG_PATH.into(),
            mount: None,
            threshold_pct: 85.0,
            min_free_bytes: None,
            min_free_pct: None,
            apply: false,
            force: false,
            json: false,
            all: false,
            limits: Limits::default(),
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
    anyhow::ensure!(
        config.threshold_pct.is_finite() && (0.0..=100.0).contains(&config.threshold_pct),
        "threshold must be between 0 and 100"
    );
    anyhow::ensure!(
        config
            .min_free_pct
            .is_none_or(|pct| pct.is_finite() && (0.0..=100.0).contains(&pct)),
        "minimum free percentage must be between 0 and 100"
    );
    anyhow::ensure!(
        config.min_free_bytes.is_none() || config.min_free_pct.is_none(),
        "choose either minimum free bytes or percentage"
    );
    anyhow::ensure!(
        (1..=86400).contains(&config.limits.timeout_seconds)
            && config.limits.gc_max_bytes > 0
            && config.limits.gc_pass_bytes > 0,
        "reclaim budgets must be positive; timeout must be at most 86400 seconds"
    );
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
                                .saturating_add(resolved.target.estimated_freed_bytes.unwrap_or(0));
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
    let totals = compute_totals(&filesystems, &unassigned_targets, config.threshold_pct);
    let health = compute_health(&filesystems, config.threshold_pct);

    Ok(ReclaimReport {
        schema_version: 3,
        threshold_pct: config.threshold_pct,
        apply: config.apply,
        force: config.force,
        totals,
        filesystems,
        unassigned_targets,
        health,
        errors: Vec::new(),
    })
}

pub fn execute(report: &mut ReclaimReport, config: &ReclaimConfig) -> Result<()> {
    if !config.apply {
        return Ok(());
    }

    let _signals = runtime::SignalGuard::install()?;
    execute_with(report, config, mount::stats, |target, context| {
        execute_target(target, config, context)
    })
}

fn execute_with(
    report: &mut ReclaimReport,
    config: &ReclaimConfig,
    mut measure: impl FnMut(&str) -> Result<mount::FilesystemStats>,
    mut action: impl FnMut(&mut ReclaimTarget, &ActionContext<'_>) -> Result<()>,
) -> Result<()> {
    let runtime = Runtime::new(&config.limits);
    for fs in &mut report.filesystems {
        refresh_filesystem_result(fs, &mut measure, &mut report.errors);
        if let Some(available) = fs.final_available_bytes {
            // Execution-time counters are the baseline; planning may have happened earlier.
            fs.available_bytes = available;
            fs.target_free_bytes = fs.required_available_bytes.saturating_sub(available);
            fs.net_available_change_bytes = Some(0);
        }
    }
    for fs in &mut report.filesystems {
        for index in 0..fs.targets.len() {
            if fs.targets[index].status != "pending" {
                continue;
            }
            let reason = runtime.stop_reason().or_else(|| {
                if fs.measurement_error.is_some() {
                    Some("filesystem availability could not be verified")
                } else if fs.goal_met == Some(true) {
                    Some("free-space goal already reached")
                } else {
                    None
                }
            });
            if let Some(reason) = reason {
                skip_target(&mut fs.targets[index], reason);
                continue;
            }
            let path = fs.mounts[0].clone();
            let context = ActionContext {
                mount: Some(&path),
                required_available_bytes: Some(fs.required_available_bytes),
                limits: &config.limits,
                runtime: &runtime,
            };
            if let Err(error) = action(&mut fs.targets[index], &context) {
                fs.targets[index].status = "error";
                fs.targets[index].notes = format!("{error:#}");
            }
            refresh_filesystem_result(fs, &mut measure, &mut report.errors);
        }
        fs.actual_freed_bytes = reported_yield(fs.targets.iter());
    }
    for target in &mut report.unassigned_targets {
        if target.status != "pending" {
            continue;
        }
        let reason = runtime.stop_reason().or_else(|| {
            if report
                .filesystems
                .iter()
                .all(|fs| fs.goal_met == Some(true))
            {
                Some("free-space goals already reached")
            } else if target.scope == TargetScope::Unassigned {
                Some("no selected backing filesystem")
            } else if report
                .filesystems
                .iter()
                .any(|fs| fs.measurement_error.is_some())
            {
                Some("filesystem availability could not be verified")
            } else {
                None
            }
        });
        if let Some(reason) = reason {
            skip_target(target, reason);
            continue;
        }
        let context = ActionContext {
            mount: None,
            required_available_bytes: None,
            limits: &config.limits,
            runtime: &runtime,
        };
        if let Err(error) = action(target, &context) {
            target.status = "error";
            target.notes = format!("{error:#}");
        }
        for fs in &mut report.filesystems {
            refresh_filesystem_result(fs, &mut measure, &mut report.errors);
        }
    }
    // Recheck every surface at completion, including effects from global actions and other writers.
    for fs in &mut report.filesystems {
        refresh_filesystem_result(fs, &mut measure, &mut report.errors);
    }
    if let Some(reason) = runtime.stop_reason() {
        report.errors.push(reason.into());
    }
    report.totals = compute_totals(
        &report.filesystems,
        &report.unassigned_targets,
        config.threshold_pct,
    );
    report.totals.total_actual_freed_bytes = reported_yield(
        report
            .filesystems
            .iter()
            .flat_map(|fs| fs.targets.iter())
            .chain(report.unassigned_targets.iter()),
    );
    report.totals.net_available_change_bytes = report
        .filesystems
        .iter()
        .map(|fs| fs.net_available_change_bytes)
        .sum();
    report.health = compute_health(&report.filesystems, config.threshold_pct);
    Ok(())
}

fn skip_target(target: &mut ReclaimTarget, reason: &str) {
    target.status = "skipped";
    target.skip_reason = Some(reason.into());
}

fn reported_yield<'a>(targets: impl Iterator<Item = &'a ReclaimTarget>) -> Option<u64> {
    targets
        .filter(|target| {
            matches!(
                target.status,
                "completed" | "completed-with-errors" | "error"
            )
        })
        .try_fold(0u64, |sum, target| {
            target
                .actual_freed_bytes
                .map(|bytes| sum.saturating_add(bytes))
        })
}

/// Called after rendering, so failed applications still produce a complete JSON/human receipt.
pub fn ensure_success(report: &ReclaimReport) -> Result<()> {
    if !report.apply {
        return Ok(());
    }
    let failures = report
        .filesystems
        .iter()
        .flat_map(|fs| fs.targets.iter())
        .chain(report.unassigned_targets.iter())
        .filter(|target| matches!(target.status, "error" | "completed-with-errors"))
        .count();
    let unmet = report
        .filesystems
        .iter()
        .filter(|fs| fs.goal_met != Some(true))
        .count();
    anyhow::ensure!(
        failures == 0 && unmet == 0 && report.errors.is_empty(),
        "reclaim incomplete: {failures} failed actions, {unmet} unmet or unverified filesystem goals, {} runtime errors",
        report.errors.len()
    );
    Ok(())
}

fn execute_target(
    target: &mut ReclaimTarget,
    config: &ReclaimConfig,
    context: &ActionContext<'_>,
) -> Result<()> {
    if target.status != "pending" {
        return Ok(());
    }

    let variant = match registry::find_variant(target.framework, target.variant) {
        Some(v) => v,
        None => {
            target.status = "error";
            target.notes = "variant not found in registry".to_string();
            return Ok(());
        }
    };

    if !config.force || variant.tier() == Tier::ReportOnly {
        match variant.tier() {
            Tier::Confirm => {
                target.status = "skipped";
                target.skip_reason = Some("confirm tier requires --force".to_string());
                return Ok(());
            }
            Tier::Risky => {
                target.status = "skipped";
                target.skip_reason = Some("risky tier requires --force".to_string());
                return Ok(());
            }
            Tier::ReportOnly => {
                target.status = "skipped";
                target.skip_reason = Some("report-only target".to_string());
                return Ok(());
            }
            Tier::Safe => {}
        }
    }

    eprintln!("reclaim: {}/{}", target.framework, target.variant);
    match variant.reclaim_with_settings(config.force, &target.settings, context) {
        Ok(apply_report) => {
            target.actual_freed_bytes = apply_report.freed_bytes;
            target.status = if apply_report.errors.is_empty() {
                "completed"
            } else {
                "completed-with-errors"
            };
            let notes = apply_report
                .notices
                .into_iter()
                .chain(apply_report.errors)
                .collect::<Vec<_>>()
                .join("; ");
            if !notes.is_empty() {
                target.notes = notes;
            }
            target.command_log = apply_report.command_log;
            Ok(())
        }
        Err(e) => {
            target.status = "error";
            target.notes = format!("{e:#}");
            Ok(())
        }
    }
}

fn empty_report(config: &ReclaimConfig) -> ReclaimReport {
    ReclaimReport {
        schema_version: 3,
        threshold_pct: config.threshold_pct,
        apply: config.apply,
        force: config.force,
        totals: ReclaimTotals::default(),
        filesystems: Vec::new(),
        unassigned_targets: Vec::new(),
        health: Vec::new(),
        errors: Vec::new(),
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

        let requested =
            requested.ok_or_else(|| anyhow::anyhow!("mount not found: {requested_mount}"))?;
        let key = fs_key(&requested);
        anyhow::ensure!(
            groups.iter().any(|group| group.key == key),
            "mount is not on an eligible backing filesystem: {requested_mount}"
        );

        if !needs_reclaim(&requested, config) {
            return Ok(Vec::new());
        }

        return Ok(vec![key]);
    }

    Ok(groups
        .iter()
        .filter(|group| needs_reclaim(&group.representative, config))
        .map(|group| group.key.clone())
        .collect())
}

fn needs_reclaim(mount: &MountInfo, config: &ReclaimConfig) -> bool {
    config.all || mount.available_bytes < required_available(mount.total_bytes, config)
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
        required_available_bytes: required_available(representative.total_bytes, config),
        estimated_freed_bytes: 0,
        actual_freed_bytes: None,
        final_available_bytes: None,
        goal_met: None,
        net_available_change_bytes: None,
        measurement_error: None,
        targets: Vec::new(),
    }
}

fn inspect_targets(
    config: &ReclaimConfig,
    groups: &[FilesystemGroup],
) -> Result<Vec<ResolvedTarget>> {
    let mut targets = Vec::new();

    for selected in crate::commands::select_variants(None, None, &config.config_path)? {
        let variant = selected.variant;
        let tier = variant.tier();
        let mut skip_reason = None;
        let mut status = "pending";
        if !config.force || tier == Tier::ReportOnly {
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

        let inspection = match variant.inspect_with_settings(&selected.settings) {
            Ok(i) => i,
            Err(error) => {
                anyhow::bail!(
                    "{}/{} inspection failed: {error:#}",
                    variant.framework().name(),
                    variant.name()
                );
            }
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
            settings: selected.settings,
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
            estimated_freed_bytes: if tier == Tier::ReportOnly {
                Some(0)
            } else {
                inspection.size_bytes
            },
            actual_freed_bytes: None,
            status,
            skip_reason,
            notes: inspection.notes,
            command_log: String::new(),
        };

        targets.push(ResolvedTarget {
            target,
            filesystem_keys: filesystem_keys.into_iter().collect(),
        });
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
    required_available(mount.total_bytes, config).saturating_sub(mount.available_bytes)
}

fn required_available(total_bytes: u64, config: &ReclaimConfig) -> u64 {
    if let Some(bytes) = config.min_free_bytes {
        return bytes;
    }

    if let Some(pct) = config.min_free_pct {
        return (total_bytes as f64 * pct / 100.0).ceil() as u64;
    }

    (total_bytes as f64 * (100.0 - config.threshold_pct) / 100.0).ceil() as u64
}

fn compute_health(filesystems: &[ReclaimFilesystem], threshold_pct: f64) -> Vec<HealthEntry> {
    filesystems
        .iter()
        .map(|fs| {
            let threshold_free = (fs.total_bytes as f64 * (100.0 - threshold_pct) / 100.0) as u64;
            let available = fs.final_available_bytes.unwrap_or(fs.available_bytes);
            let below_threshold = available >= threshold_free;
            let needed_bytes = if below_threshold {
                0
            } else {
                threshold_free.saturating_sub(available)
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
    threshold_pct: f64,
) -> ReclaimTotals {
    let fs_target_count: usize = filesystems.iter().map(|fs| fs.targets.len()).sum();
    let fs_estimated: u64 = filesystems.iter().map(|fs| fs.estimated_freed_bytes).sum();
    let unassigned_estimated: u64 = unassigned_targets
        .iter()
        .filter(|target| target.status != "skipped")
        .map(|target| target.estimated_freed_bytes.unwrap_or(0))
        .sum();
    let skipped_estimated_freed_bytes: u64 = filesystems
        .iter()
        .flat_map(|fs| fs.targets.iter())
        .chain(unassigned_targets.iter())
        .filter(|target| target.status == "skipped")
        .map(|target| target.estimated_freed_bytes.unwrap_or(0))
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
            if let Some(available) = fs.final_available_bytes {
                fs.required_available_bytes.saturating_sub(available)
            } else {
                fs.target_free_bytes
                    .saturating_sub(fs.estimated_freed_bytes)
            }
        })
        .sum();

    ReclaimTotals {
        filesystems_total: filesystems.len(),
        filesystems_over_threshold: filesystems
            .iter()
            .filter(|fs| fs.usage_pct >= threshold_pct)
            .count(),
        targets_total: fs_target_count + unassigned_targets.len(),
        skipped_targets,
        total_estimated_freed_bytes: fs_estimated.saturating_add(unassigned_estimated),
        skipped_estimated_freed_bytes,
        total_target_free_bytes,
        total_goal_shortfall_bytes,
        total_actual_freed_bytes: None,
        unknown_estimates: filesystems
            .iter()
            .flat_map(|fs| fs.targets.iter())
            .chain(unassigned_targets.iter())
            .filter(|target| target.status != "skipped" && target.estimated_freed_bytes.is_none())
            .count(),
        net_available_change_bytes: None,
    }
}

fn goal_met(fs: &ReclaimFilesystem, available: u64) -> bool {
    available >= fs.required_available_bytes
}

fn refresh_filesystem_result(
    fs: &mut ReclaimFilesystem,
    measure: &mut impl FnMut(&str) -> Result<mount::FilesystemStats>,
    errors: &mut Vec<String>,
) {
    let result = fs
        .mounts
        .first()
        .ok_or_else(|| anyhow::anyhow!("filesystem has no mount path"))
        .and_then(|path| measure(path));
    if let Ok(df) = result {
        fs.final_available_bytes = Some(df.available_bytes);
        fs.goal_met = Some(goal_met(fs, df.available_bytes));
        fs.used_bytes = df.used_bytes;
        fs.usage_pct = if df.total_bytes == 0 {
            0.0
        } else {
            df.used_bytes as f64 / df.total_bytes as f64 * 100.0
        };
        fs.net_available_change_bytes =
            Some(i128::from(df.available_bytes) - i128::from(fs.available_bytes));
        fs.measurement_error = None;
    } else {
        fs.final_available_bytes = None;
        fs.goal_met = None;
        fs.net_available_change_bytes = None;
        fs.measurement_error = result.err().map(|error| format!("{error:#}"));
        if let Some(error) = &fs.measurement_error {
            let message = format!("{} measurement failed: {error}", fs.device);
            if !errors.contains(&message) {
                errors.push(message);
            }
        }
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
    format!(
        "{}\u{1f}{}\u{1f}{}",
        mount.device, mount.maj_min, mount.fstype
    )
}

fn fs_key_from_report(fs: &ReclaimFilesystem) -> String {
    format!("{}\u{1f}{}\u{1f}{}", fs.device, fs.maj_min, fs.fstype)
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
            "All selected filesystems satisfy the free-space goal. Nothing to reclaim (threshold: {:.1}%).\n",
            report.threshold_pct,
        ));
        return output;
    }

    output.push_str("Reclaim summary\n");
    output.push_str(&format!(
        "  Filesystems: {} selected  Targets: {} total ({} skipped)\n",
        report.totals.filesystems_total, report.totals.targets_total, report.totals.skipped_targets
    ));
    output.push_str(&format!(
        "  Known estimated reclaim: {} (+{} unknown yields)  Additional space sought: {}\n  {}: {}\n\n",
        human_size(report.totals.total_estimated_freed_bytes),
        report.totals.unknown_estimates,
        human_size(report.totals.total_target_free_bytes),
        if report.apply && report.filesystems.iter().all(|fs| fs.final_available_bytes.is_some()) { "Measured remaining shortfall" } else if report.apply { "Unverified shortfall estimate" } else { "Shortfall after known estimates" },
        human_size(report.totals.total_goal_shortfall_bytes)
    ));
    if report.totals.skipped_estimated_freed_bytes > 0 {
        output.push_str(&format!(
            "  Gated by tier policy: {} (use --force to include configured confirm/risky targets; report-only targets never reclaim space)\n\n",
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
            "  Size: {}  Used: {}  Initial available: {}  Required available: {} ({} bytes)\n  Known estimated reclaim: {}\n",
            human_size(fs.total_bytes),
            human_size(fs.used_bytes),
            human_size(fs.available_bytes),
            human_size(fs.required_available_bytes),
            fs.required_available_bytes,
            human_size(fs.estimated_freed_bytes)
        ));

        if fs.targets.is_empty() {
            output.push_str("  No configured filesystem-local targets found.\n");
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

        if report.apply {
            let goal = match fs.goal_met {
                Some(true) => "MET",
                Some(false) => "NOT MET",
                None => "UNVERIFIED",
            };
            output.push_str(&format!(
                "\n  Result: goal {goal}; adapter-reported yield: {}\n",
                optional_size(fs.actual_freed_bytes)
            ));
            if let Some(net) = fs.net_available_change_bytes {
                output.push_str(&format!("  Net available-space change: {net:+} bytes\n"));
            }
            if let Some(final_avail) = fs.final_available_bytes {
                output.push_str(&format!("  Final available: {} ({final_avail} bytes)\n  Remaining shortfall: {} ({} bytes)\n", human_size(final_avail), human_size(fs.required_available_bytes.saturating_sub(final_avail)), fs.required_available_bytes.saturating_sub(final_avail)));
            }
            if let Some(error) = &fs.measurement_error {
                output.push_str(&format!("  Measurement failed: {error}\n"));
            }
            if fs.goal_met == Some(false) {
                output.push_str("  Eligible actions exhausted or stopped at their configured budgets; see target notes.\n");
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
        output.push_str("Threshold health (separate from the requested free-space goal)\n");
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

    for error in &report.errors {
        output.push_str(&format!("Reclaim error: {error}\n"));
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
        optional_size(target.estimated_freed_bytes),
        target.status,
        truncate(&surfaces, 18),
        notes
    )
}

fn optional_size(bytes: Option<u64>) -> String {
    bytes.map(human_size).unwrap_or_else(|| "unknown".into())
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

    #[test]
    fn explicit_free_goal_selects_filesystem_below_default_threshold() {
        let groups = filesystem_groups(&[mount("/nix", "test", 1000, 700)]);
        let config = ReclaimConfig {
            mount: Some("/nix".into()),
            min_free_bytes: Some(400),
            ..Default::default()
        };
        assert_eq!(selected_filesystem_keys(&config, &groups).unwrap().len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn undiscovered_requested_filesystem_cannot_be_a_successful_empty_plan() {
        let config = ReclaimConfig {
            mount: Some("/nix".into()),
            min_free_bytes: Some(u64::MAX),
            ..Default::default()
        };
        assert!(selected_filesystem_keys(&config, &[]).is_err());
    }

    #[test]
    fn btrfs_subvolumes_share_one_reclaim_goal() {
        let mut root = mount("/", "test", 1000, 900);
        root.fsroot = Some("/@root".into());
        let mut nix = root.clone();
        nix.mount_point = "/nix".into();
        nix.fsroot = Some("/@nix".into());
        assert_eq!(filesystem_groups(&[root, nix]).len(), 1);
    }

    fn execution_fixture(required: u64, initial: u64) -> (ReclaimConfig, ReclaimReport) {
        let config = ReclaimConfig {
            apply: true,
            min_free_bytes: Some(required),
            ..Default::default()
        };
        let groups = filesystem_groups(&[mount(
            "/nix",
            "test",
            required * 10,
            required * 10 - initial,
        )]);
        let mut report = empty_report(&config);
        report
            .filesystems
            .push(filesystem_report(&groups[0], &config));
        (config, report)
    }

    #[test]
    fn stops_after_goal_and_keeps_unknown_yield_distinct_from_zero() {
        let (config, mut report) = execution_fixture(150, 40);
        report.filesystems[0].targets = vec![
            sample_target("first", "clean", "/nix"),
            sample_target("second", "clean", "/nix"),
        ];
        report.filesystems[0].targets[0].estimated_freed_bytes = None;
        let available = std::cell::Cell::new(40);
        let mut actions = 0;
        execute_with(
            &mut report,
            &config,
            |_| {
                Ok(mount::FilesystemStats {
                    total_bytes: 1500,
                    used_bytes: 1500 - available.get(),
                    available_bytes: available.get(),
                })
            },
            |target, _| {
                actions += 1;
                available.set(160);
                target.status = "completed";
                target.actual_freed_bytes = None;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(actions, 1);
        assert_eq!(report.filesystems[0].targets[1].status, "skipped");
        assert_eq!(report.filesystems[0].net_available_change_bytes, Some(120));
        assert_eq!(report.totals.total_actual_freed_bytes, None);
        assert_eq!(report.totals.total_goal_shortfall_bytes, 0);
        assert!(ensure_success(&report).is_ok());
        let human = format_report_human(&report);
        assert!(human.contains("goal MET; adapter-reported yield: unknown"));
        assert!(human.contains("160 bytes"));
    }

    #[test]
    fn exact_100_gib_goal_is_unmet_even_when_display_rounds_to_100() {
        let required = 100 << 30;
        let initial = required - (100 << 20);
        let (config, mut report) = execution_fixture(required, initial);
        execute_with(
            &mut report,
            &config,
            |_| {
                Ok(mount::FilesystemStats {
                    total_bytes: required * 10,
                    used_bytes: required * 10 - initial,
                    available_bytes: initial,
                })
            },
            |_, _| panic!("no configured cleanup action"),
        )
        .unwrap();
        assert!(ensure_success(&report).is_err());
        assert_eq!(report.filesystems[0].goal_met, Some(false));
        assert_eq!(report.totals.total_goal_shortfall_bytes, 100 << 20);
        let human = format_report_human(&report);
        assert!(human.contains("goal NOT MET"));
        assert!(human.contains("104857600 bytes"));
        assert!(human.contains("No configured filesystem-local targets"));
    }

    #[test]
    fn failed_action_still_fails_when_other_writers_reach_goal() {
        let (config, mut report) = execution_fixture(150, 40);
        report.filesystems[0]
            .targets
            .push(sample_target("first", "clean", "/nix"));
        let available = std::cell::Cell::new(40);
        execute_with(
            &mut report,
            &config,
            |_| {
                Ok(mount::FilesystemStats {
                    total_bytes: 1500,
                    used_bytes: 1500 - available.get(),
                    available_bytes: available.get(),
                })
            },
            |_, _| {
                available.set(150);
                anyhow::bail!("sudo: a password is required")
            },
        )
        .unwrap();
        assert_eq!(report.filesystems[0].goal_met, Some(true));
        assert!(ensure_success(&report).is_err());
        assert!(
            report.filesystems[0].targets[0]
                .notes
                .contains("password is required")
        );
    }

    #[test]
    fn measurement_failure_skips_cleanup_and_keeps_result_unverified() {
        let (config, mut report) = execution_fixture(150, 40);
        report.filesystems[0]
            .targets
            .push(sample_target("first", "clean", "/nix"));
        execute_with(
            &mut report,
            &config,
            |_| anyhow::bail!("statvfs unavailable"),
            |_, _| panic!("cleanup requires a verified goal"),
        )
        .unwrap();
        assert_eq!(report.filesystems[0].goal_met, None);
        assert_eq!(report.filesystems[0].targets[0].status, "skipped");
        assert!(ensure_success(&report).is_err());
        assert!(format_report_human(&report).contains("UNVERIFIED"));
    }

    #[test]
    fn execution_baseline_and_already_met_goal_prevent_unnecessary_cleanup() {
        let (config, mut report) = execution_fixture(150, 40);
        report.filesystems[0]
            .targets
            .push(sample_target("first", "clean", "/nix"));
        execute_with(
            &mut report,
            &config,
            |_| {
                Ok(mount::FilesystemStats {
                    total_bytes: 1500,
                    used_bytes: 1300,
                    available_bytes: 200,
                })
            },
            |_, _| panic!("goal met before execution"),
        )
        .unwrap();
        assert_eq!(report.filesystems[0].available_bytes, 200);
        assert_eq!(report.filesystems[0].net_available_change_bytes, Some(0));
        assert!(ensure_success(&report).is_ok());
    }

    #[test]
    fn signed_net_change_is_preserved_when_concurrent_writes_exceed_cleanup() {
        let (config, mut report) = execution_fixture(150, 40);
        report.filesystems[0]
            .targets
            .push(sample_target("first", "clean", "/nix"));
        let available = std::cell::Cell::new(40);
        execute_with(
            &mut report,
            &config,
            |_| {
                Ok(mount::FilesystemStats {
                    total_bytes: 1500,
                    used_bytes: 1500 - available.get(),
                    available_bytes: available.get(),
                })
            },
            |target, _| {
                available.set(30);
                target.status = "completed";
                target.actual_freed_bytes = Some(100);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(report.totals.total_actual_freed_bytes, Some(100));
        assert_eq!(report.totals.net_available_change_bytes, Some(-10));
        assert!(ensure_success(&report).is_err());
    }

    #[test]
    fn configured_reclaim_preserves_pins_and_never_executes_reports() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        std::fs::create_dir(&models).unwrap();
        std::fs::write(models.join("pinned.gguf"), b"keep").unwrap();
        std::fs::write(models.join("unused.gguf"), b"remove").unwrap();
        let settings = serde_json::json!({"modelsDir": models, "pinnedFiles": ["pinned.gguf"]});
        let policy = dir.path().join("targets.json");
        std::fs::write(
            &policy,
            serde_json::json!({"targets": [
                {"name": "llama-models", "variant": "prune-unpinned", "settings": settings},
                {"name": "llama-models", "variant": "disk-report", "settings": settings}
            ]})
            .to_string(),
        )
        .unwrap();
        let mut config = ReclaimConfig {
            config_path: policy.to_str().unwrap().into(),
            ..Default::default()
        };
        let groups = filesystem_groups(&[mount("/", "test", 1000, 960)]);
        let gated = inspect_targets(&config, &groups).unwrap();
        assert_eq!(gated.len(), 2);
        assert_eq!(gated[0].target.status, "skipped");
        assert_eq!(gated[0].target.estimated_freed_bytes, Some(6));
        assert_eq!(gated[1].target.estimated_freed_bytes, Some(0));
        config.force = true;
        let mut targets = inspect_targets(&config, &groups).unwrap();
        assert_eq!(targets[1].target.status, "skipped");
        // Even a mistakenly pending report remains non-executable with force.
        targets[1].target.status = "pending";
        let runtime = Runtime::new(&config.limits);
        let context = ActionContext {
            mount: None,
            required_available_bytes: None,
            limits: &config.limits,
            runtime: &runtime,
        };
        execute_target(&mut targets[1].target, &config, &context).unwrap();
        assert_eq!(targets[1].target.status, "skipped");
        execute_target(&mut targets[0].target, &config, &context).unwrap();
        assert_eq!(targets[0].target.actual_freed_bytes, Some(6));
        assert!(models.join("pinned.gguf").exists());
        assert!(!models.join("unused.gguf").exists());
        std::fs::write(&policy, r#"{"targets": []}"#).unwrap();
        assert!(inspect_targets(&config, &groups).unwrap().is_empty());
    }

    #[test]
    fn threshold_goal_and_post_cleanup_health_agree() {
        let config = ReclaimConfig::default();
        let groups = filesystem_groups(&[mount("/", "test", 1000, 960)]);
        let mut fs = filesystem_report(&groups[0], &config);
        assert_eq!(fs.target_free_bytes, 110);
        assert!(!goal_met(&fs, 110));
        assert!(goal_met(&fs, 150));
        fs.final_available_bytes = Some(150);
        let health = compute_health(&[fs], 85.0);
        assert!(health[0].below_threshold);
        assert_eq!(health[0].needed_bytes, 0);
        assert_eq!(
            compute_target_free(
                &groups[0].representative,
                &ReclaimConfig {
                    min_free_bytes: Some(200),
                    ..config
                }
            ),
            160
        );
    }

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
            settings: serde_json::Value::Null,
            framework,
            variant,
            tier: "safe",
            scope: TargetScope::SingleSurface,
            paths: vec![format!("{surface}/cache")],
            surfaces: vec![surface.to_string()],
            estimated_freed_bytes: Some(1024),
            actual_freed_bytes: None,
            status: "pending",
            skip_reason: None,
            notes: "test target".to_string(),
            command_log: String::new(),
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
            schema_version: 3,
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
                unknown_estimates: 0,
                net_available_change_bytes: None,
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
                required_available_bytes: 10,
                estimated_freed_bytes: 1024,
                actual_freed_bytes: None,
                final_available_bytes: None,
                goal_met: None,
                net_available_change_bytes: None,
                measurement_error: None,
                targets: vec![sample_target("user-cache", "purge-go-build", "/home")],
            }],
            unassigned_targets: vec![ReclaimTarget {
                settings: serde_json::Value::Null,
                framework: "failed-units",
                variant: "reset",
                tier: "safe",
                scope: TargetScope::Unassigned,
                paths: vec!["systemctl --failed".to_string()],
                surfaces: Vec::new(),
                estimated_freed_bytes: Some(1024),
                actual_freed_bytes: None,
                status: "pending",
                skip_reason: None,
                notes: "system action".to_string(),
                command_log: String::new(),
            }],
            health: Vec::new(),
            errors: Vec::new(),
        };

        let formatted = format_report_human(&report);

        assert!(formatted.contains("Mounts: /, /home, /nix, /.snapshots"));
        assert_eq!(formatted.matches("user-cache/purge-go-build").count(), 1);
        assert!(formatted.contains("Global and unassigned actions"));
        assert!(formatted.contains("failed-units/reset"));
    }

    #[test]
    fn report_serializes_schema_version_three() {
        let report = empty_report(&ReclaimConfig::default());
        let json = serde_json::to_value(report).unwrap();

        assert_eq!(json["schema_version"], 3);
        assert!(json.get("filesystems").is_some());
        assert!(json.get("unassigned_targets").is_some());
        assert!(json.get("totals").is_some());
        assert!(json.get("health").is_some());
    }
}
