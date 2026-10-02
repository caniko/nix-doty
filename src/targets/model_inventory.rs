//! Receipts establish discovery, never deletion authority. Old/unknown artifacts
//! remain visible when their profile disappears or inspection is blocked.
use super::*;
use serde::Serialize;

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Lifecycle {
    Active,
    #[default]
    Retained,
    Retired,
    Orphaned,
    Partial,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Model {
    id: String,
    path: PathBuf,
    repo: String,
    rev: String,
    #[serde(default)]
    lifecycle: Lifecycle,
    #[serde(default)]
    retired_at: Option<String>,
    #[serde(default)]
    retain_until: Option<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    lock_path: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct InventorySettings {
    roots: Vec<PathBuf>,
    models: Vec<Model>,
    pinned_paths: Vec<PathBuf>,
    min_age_days: Option<u32>,
    max_entries: Option<u64>,
    max_depth: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    schema_version: u32,
    kind: String,
    repo: String,
    rev: String,
    files: Vec<ReceiptFile>,
    total_bytes: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReceiptFile {
    name: String,
    size_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Artifact {
    path: PathBuf,
    manifest: Option<PathBuf>,
    model_ids: Vec<String>,
    repo: Option<String>,
    rev: Option<String>,
    lifecycle: Lifecycle,
    logical_bytes: Option<u64>,
    allocated_bytes: Option<u64>,
    eligible: bool,
    blockers: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Inventory {
    schema_version: u32,
    complete: bool,
    artifacts: Vec<Artifact>,
    issues: Vec<String>,
}

fn safe_relative(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && Path::new(name)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "receipt file must be a safe relative path: {name}"
    );
    ensure!(
        !Path::new(name)
            .components()
            .any(|c| excluded(c.as_os_str())),
        "excluded receipt file: {name}"
    );
    Ok(())
}

fn receipt(path: &Path) -> Result<Receipt> {
    use std::io::Read;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && meta.len() <= 16 * 1024 * 1024,
        "invalid or oversized model receipt"
    );
    let mut bytes = Vec::new();
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16 * 1024 * 1024, "oversized model receipt");
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    ensure!(
        receipt.schema_version == 1 && !receipt.repo.is_empty() && !receipt.rev.is_empty(),
        "unsupported model receipt identity"
    );
    ensure!(
        matches!(receipt.kind.as_str(), "colibri-dir" | "gguf-file"),
        "unsupported model receipt layout"
    );
    ensure!(
        !receipt.files.is_empty() && receipt.files.len() <= 100_000,
        "invalid model receipt inventory"
    );
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for file in &receipt.files {
        safe_relative(&file.name)?;
        ensure!(names.insert(&file.name), "duplicate model receipt file");
        total = total
            .checked_add(file.size_bytes)
            .context("model receipt size overflow")?;
    }
    ensure!(
        total == receipt.total_bytes,
        "model receipt total differs from file inventory"
    );
    Ok(receipt)
}

fn discover(
    root: &Path,
    remaining: &mut u64,
    depth: u32,
    manifests: &mut BTreeSet<PathBuf>,
    loose: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    real_directory(root)?;
    let device = fs::symlink_metadata(root)?.dev();
    for path in children(root, remaining)? {
        if path.file_name().is_some_and(excluded) {
            continue;
        }
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            !meta.is_symlink() && meta.dev() == device,
            "unobservable model entry: {}",
            path.display()
        );
        if meta.is_file() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name == "ready.json" || (name.starts_with(".ready-") && name.ends_with(".json")) {
                manifests.insert(path);
            } else if name.ends_with(".gguf") {
                loose.insert(path);
            }
        } else if meta.is_dir() && depth > 0 {
            discover(&path, remaining, depth - 1, manifests, loose)?;
        } else if !meta.is_dir() {
            anyhow::bail!("special model entry: {}", path.display());
        }
    }
    Ok(())
}

fn timestamp(value: &str) -> Result<SystemTime> {
    Ok(chrono::DateTime::parse_from_rfc3339(value)?.into())
}

impl InventorySettings {
    fn policy(&self) -> Result<Settings> {
        Settings::parse(
            &serde_json::json!({ "roots": self.roots, "pinnedPaths": self.pinned_paths,
            "minAgeDays": self.min_age_days.unwrap_or(7), "maxEntries": self.max_entries.unwrap_or(1_000_000), "maxDepth": self.max_depth.unwrap_or(64) }),
        )
    }

    fn inventory(&self) -> Result<Inventory> {
        let policy = self.policy()?;
        for model in &self.models {
            ensure!(
                model.path.is_absolute()
                    && !model
                        .path
                        .components()
                        .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
                "invalid declared model path"
            );
            ensure!(
                self.roots.iter().any(|root| model.path.starts_with(root)),
                "model outside discovery roots"
            );
            for name in &model.files {
                safe_relative(name)?;
            }
        }
        let mut result = Inventory {
            schema_version: 1,
            complete: true,
            artifacts: vec![],
            issues: vec![],
        };
        let mut manifests = BTreeSet::new();
        let mut loose = BTreeSet::new();
        for root in &self.roots {
            let mut remaining = policy.max_entries;
            if let Err(error) = discover(root, &mut remaining, 1, &mut manifests, &mut loose) {
                result.complete = false;
                result.issues.push(format!("{}: {error}", root.display()));
            }
        }
        for manifest in manifests {
            let base = manifest.parent().context("receipt has no parent")?;
            match receipt(&manifest) {
                Ok(receipt) => {
                    let paths = if receipt.kind == "colibri-dir" {
                        vec![base.to_path_buf()]
                    } else {
                        receipt
                            .files
                            .iter()
                            .map(|file| base.join(&file.name))
                            .collect()
                    };
                    for path in paths {
                        loose.remove(&path);
                        let mut artifact =
                            self.artifact(path, Some(manifest.clone()), Some(&receipt), &policy);
                        for file in &receipt.files {
                            let path = base.join(&file.name);
                            let verified = real_directory(path.parent().unwrap_or(base)).is_ok()
                                && fs::symlink_metadata(&path).is_ok_and(|meta| {
                                    meta.is_file() && meta.len() == file.size_bytes
                                });
                            if !verified {
                                artifact.blockers.push(format!(
                                    "missing/changed receipt file: {}",
                                    path.display()
                                ));
                            }
                        }
                        artifact.eligible &= artifact.blockers.is_empty();
                        if !artifact.blockers.is_empty() {
                            result.complete = false;
                        }
                        result.artifacts.push(artifact);
                    }
                }
                Err(error) => {
                    result.complete = false;
                    result.artifacts.push(Artifact {
                        path: base.to_path_buf(),
                        manifest: Some(manifest),
                        model_ids: vec![],
                        repo: None,
                        rev: None,
                        lifecycle: Lifecycle::Partial,
                        logical_bytes: None,
                        allocated_bytes: None,
                        eligible: false,
                        blockers: vec![error.to_string()],
                    });
                }
            }
        }
        loose.extend(self.models.iter().flat_map(|m| {
            if m.files.is_empty() {
                vec![m.path.clone()]
            } else {
                m.files.iter().map(|f| m.path.join(f)).collect()
            }
        }));
        for path in loose {
            if result.artifacts.iter().any(|a| a.path == path) {
                continue;
            }
            let artifact = self.artifact(path, None, None, &policy);
            if !artifact.blockers.is_empty() {
                result.complete = false;
            }
            result.artifacts.push(artifact);
        }
        result.artifacts.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(result)
    }

    fn artifact(
        &self,
        path: PathBuf,
        manifest: Option<PathBuf>,
        receipt: Option<&Receipt>,
        policy: &Settings,
    ) -> Artifact {
        let models: Vec<_> = self
            .models
            .iter()
            .filter(|m| {
                if m.files.is_empty() {
                    m.path == path
                } else {
                    m.files.iter().any(|f| m.path.join(f) == path)
                }
            })
            .collect();
        let pinned = policy
            .pinned_paths
            .iter()
            .any(|pin| path.starts_with(pin) || pin.starts_with(&path))
            || models.iter().any(|m| m.lifecycle == Lifecycle::Active);
        let lifecycle = if pinned {
            Lifecycle::Active
        } else if models.is_empty() {
            Lifecycle::Orphaned
        } else if models.iter().all(|m| m.lifecycle == Lifecycle::Retired) {
            Lifecycle::Retired
        } else {
            Lifecycle::Retained
        };
        let mut artifact = Artifact {
            path,
            manifest,
            model_ids: models.iter().map(|m| m.id.clone()).collect(),
            repo: receipt.map(|r| r.repo.clone()),
            rev: receipt.map(|r| r.rev.clone()),
            lifecycle,
            logical_bytes: None,
            allocated_bytes: None,
            eligible: false,
            blockers: vec![],
        };
        let scan = policy.root_for(&artifact.path).and_then(|root| {
            guard_retention_path(&artifact.path, root)?;
            let mut remaining = policy.max_entries;
            scan_tree(&artifact.path, policy, &mut remaining)
        });
        match scan {
            Ok(scan) => {
                artifact.logical_bytes = Some(scan.logical_bytes);
                artifact.allocated_bytes = Some(scan.allocated_bytes);
                if !scan.complete {
                    artifact
                        .blockers
                        .push("incomplete/protected artifact scan".into());
                }
            }
            Err(error) => artifact.blockers.push(error.to_string()),
        }
        if let Some(receipt) = receipt {
            if models
                .iter()
                .any(|m| m.repo != receipt.repo || m.rev != receipt.rev)
            {
                artifact
                    .blockers
                    .push("receipt differs from declared repository/revision".into());
            }
        } else {
            artifact
                .blockers
                .push("no verified download receipt".into());
        }
        if lifecycle != Lifecycle::Retired {
            return artifact;
        }
        if let Err(error) = self.retirement_policy(&models, &artifact.path, policy) {
            artifact.blockers.push(error.to_string());
            return artifact;
        }
        let mut selected_policy = Settings {
            roots: policy.roots.clone(),
            pinned_paths: policy.pinned_paths.clone(),
            min_age_days: policy.min_age_days,
            max_entries: policy.max_entries,
            max_depth: policy.max_depth,
            ..Settings::default()
        };
        selected_policy.paths = vec![artifact.path.clone()];
        selected_policy.retired_paths = selected_policy.paths.clone();
        match select(Kind::Models, &selected_policy) {
            Ok(selection) => {
                artifact.eligible =
                    !selection.candidates.is_empty() && artifact.blockers.is_empty();
                artifact.blockers.extend(selection.skipped);
            }
            Err(error) => artifact.blockers.push(error.to_string()),
        }
        artifact
    }

    fn retirement_policy(&self, models: &[&Model], path: &Path, policy: &Settings) -> Result<()> {
        ensure!(
            fs::symlink_metadata(path)?.is_dir(),
            "shared-directory model files are report-only"
        );
        let now = SystemTime::now();
        let retention = Duration::from_secs(u64::from(policy.min_age_days) * 86400);
        for model in models {
            let retired = timestamp(
                model
                    .retired_at
                    .as_deref()
                    .context("retirement has no typed retiredAt")?,
            )?;
            ensure!(
                now.duration_since(retired)
                    .is_ok_and(|age| age >= retention),
                "retirement retention has not elapsed"
            );
            if let Some(until) = &model.retain_until {
                ensure!(now >= timestamp(until)?, "retainUntil has not elapsed");
            }
            ensure!(
                model.lock_path.is_some(),
                "download/cleanup lock is not configured"
            );
        }
        Ok(())
    }
}

struct ManagedModels;
static FRAMEWORK: ManagedModels = ManagedModels;
pub static MANAGED_MODELS: &dyn Framework = &FRAMEWORK;
impl Framework for ManagedModels {
    fn name(&self) -> &'static str {
        "managed-models"
    }
    fn summary(&self) -> &'static str {
        "Downloaded model receipts reconciled with lifecycle, retention and consumer pins"
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        VARIANTS
    }
}
struct ManagedVariant {
    prune: bool,
}
static INVENTORY: ManagedVariant = ManagedVariant { prune: false };
static PRUNE: ManagedVariant = ManagedVariant { prune: true };
static VARIANTS: &[&dyn Variant] = &[&INVENTORY, &PRUNE];
impl Variant for ManagedVariant {
    fn name(&self) -> &'static str {
        if self.prune {
            "prune-retired"
        } else {
            "inventory"
        }
    }
    fn framework(&self) -> &'static dyn Framework {
        &FRAMEWORK
    }
    fn tier(&self) -> Tier {
        if self.prune {
            Tier::Confirm
        } else {
            Tier::ReportOnly
        }
    }
    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&serde_json::json!({}))
    }
    fn apply(&self, apply: bool, force: bool) -> Result<ApplyReport> {
        self.apply_with_settings(apply, force, &serde_json::json!({}))
    }
    fn inspect_with_settings(&self, value: &Value) -> Result<Inspection> {
        let settings: InventorySettings = serde_json::from_value(value.clone())?;
        let inventory = settings.inventory()?;
        Ok(Inspection {
            framework: FRAMEWORK.name(),
            variant: self.name(),
            path: settings
                .roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: if self.prune {
                inventory.artifacts.iter().filter(|a| a.eligible).count() as u64
            } else {
                0
            },
            notes: serde_json::to_string(&inventory)?,
        })
    }
    fn apply_with_settings(&self, apply: bool, force: bool, value: &Value) -> Result<ApplyReport> {
        let settings: InventorySettings = serde_json::from_value(value.clone())?;
        let inventory = settings.inventory()?;
        let mut policy = settings.policy()?;
        policy.paths = inventory
            .artifacts
            .iter()
            .filter(|a| a.eligible)
            .map(|a| a.path.clone())
            .collect();
        policy.retired_paths = policy.paths.clone();
        let mut locks = vec![];
        if apply && force && self.prune {
            let lock_paths: BTreeSet<_> = settings
                .models
                .iter()
                .filter(|m| policy.paths.contains(&m.path))
                .filter_map(|m| m.lock_path.as_deref())
                .collect();
            for path in lock_paths {
                locks.push(crate::model_lock::exclusive(path)?);
            }
            let fresh = settings.inventory()?;
            ensure!(
                policy
                    .paths
                    .iter()
                    .all(|p| fresh.artifacts.iter().any(|a| &a.path == p && a.eligible)),
                "model inventory changed before cleanup"
            );
        }
        let mut report = MODEL_PRUNE.apply_settings(apply && self.prune, force, &policy)?;
        report.framework = FRAMEWORK.name();
        report.variant = self.name();
        drop(locks);
        Ok(report)
    }
}
