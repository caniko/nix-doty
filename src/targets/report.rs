use crate::framework::{ApplyReport, Inspection, Variant};
use serde::Serialize;
use serde_json::Value;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(crate) fn paths_from_settings(settings: &Value, defaults: &[&str]) -> Vec<String> {
    let paths: Vec<String> = settings
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect();
    if paths.is_empty() {
        defaults.iter().map(|p| (*p).to_string()).collect()
    } else {
        paths
    }
}

pub(crate) fn report_only_apply(v: &dyn Variant) -> ApplyReport {
    ApplyReport {
        framework: v.framework().name(),
        variant: v.name(),
        removed: 0,
        freed_bytes: 0,
        skipped: 0,
        errors: vec![],
    }
}

pub(crate) fn inspect_paths(
    framework: &'static str,
    variant: &'static str,
    defaults: &[&str],
    settings: &Value,
    summary: &str,
) -> Inspection {
    let paths = paths_from_settings(settings, defaults);
    let usages: Vec<_> = paths
        .iter()
        .map(|p| path_usage(Path::new(p), 20_000))
        .collect();
    let complete = usages.iter().all(|p| p.complete);
    Inspection {
        framework,
        variant,
        path: paths.join(", "),
        size_bytes: complete.then(|| usages.iter().map(|p| p.logical_bytes_lower_bound).sum()),
        age_oldest_days: None,
        would_remove: 0,
        notes: serde_json::json!({"summary": summary, "complete": complete, "paths": usages})
            .to_string(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PathUsage {
    pub path: PathBuf,
    pub complete: bool,
    pub logical_bytes_lower_bound: u64,
    pub allocated_bytes_lower_bound: u64,
    pub issues: Vec<String>,
}

pub(crate) fn path_usage(path: &Path, max_entries: u64) -> PathUsage {
    let mut result = PathUsage {
        path: path.into(),
        complete: true,
        logical_bytes_lower_bound: 0,
        allocated_bytes_lower_bound: 0,
        issues: vec![],
    };
    for ancestor in path.ancestors().skip(1) {
        if !std::fs::symlink_metadata(ancestor).is_ok_and(|meta| meta.is_dir()) {
            result.complete = false;
            result.issues.push(format!(
                "unobservable or symlinked ancestor: {}",
                ancestor.display()
            ));
            return result;
        }
    }
    let mut pending = vec![(path.to_path_buf(), 0)];
    let mut remaining = max_entries;
    let mut device = None;
    let mut inodes = std::collections::BTreeSet::new();
    while let Some((path, depth)) = pending.pop() {
        let scan = (|| -> anyhow::Result<()> {
            anyhow::ensure!(remaining > 0 && depth <= 64, "scan budget exhausted");
            remaining -= 1;
            let meta = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(!meta.is_symlink(), "symlink not followed");
            anyhow::ensure!(
                *device.get_or_insert(meta.dev()) == meta.dev(),
                "mount boundary not crossed"
            );
            if meta.is_file() {
                result.logical_bytes_lower_bound =
                    result.logical_bytes_lower_bound.saturating_add(meta.len());
                if inodes.insert((meta.dev(), meta.ino())) {
                    result.allocated_bytes_lower_bound = result
                        .allocated_bytes_lower_bound
                        .saturating_add(meta.blocks().saturating_mul(512));
                }
            } else if meta.is_dir() {
                for entry in std::fs::read_dir(&path)? {
                    let entry = entry?;
                    // Persistent scans never enter temporary or custody trees.
                    if ["tmp", ".tmp", ".doty-quarantine"]
                        .iter()
                        .any(|n| entry.file_name() == *n)
                    {
                        result.complete = false;
                        if result.issues.len() < 32 {
                            result
                                .issues
                                .push(format!("excluded subtree: {}", entry.path().display()));
                        }
                        continue;
                    }
                    anyhow::ensure!(pending.len() < remaining as usize, "scan budget exhausted");
                    pending.push((entry.path(), depth + 1));
                }
            } else {
                anyhow::bail!("special entry not scanned");
            }
            Ok(())
        })();
        if let Err(error) = scan {
            result.complete = false;
            if result.issues.len() < 32 {
                result.issues.push(format!("{}: {error}", path.display()));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_path_is_unknown_while_known_sibling_remains_measured() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("payload");
        std::fs::write(&file, b"known").unwrap();
        let missing = root.path().join("missing");
        let report = inspect_paths(
            "fixture",
            "report",
            &[],
            &serde_json::json!({"paths": [file, missing]}),
            "fixture",
        );
        assert_eq!(report.size_bytes, None);
        let notes: Value = serde_json::from_str(&report.notes).unwrap();
        assert_eq!(notes["complete"], false);
        assert_eq!(notes["paths"][0]["logicalBytesLowerBound"], 5);
        assert!(notes["paths"][1]["issues"].to_string().contains("missing"));
    }
    #[test]
    fn symlink_and_mount_scans_never_escape_the_configured_path() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/", root.path().join("outside")).unwrap();
        let usage = path_usage(root.path(), 20);
        assert!(!usage.complete);
        assert_eq!(usage.logical_bytes_lower_bound, 0);
        assert!(usage.issues[0].contains("symlink"));
    }

    #[test]
    fn excluded_temporary_storage_is_an_explicit_incomplete_lower_bound() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("known"), b"known").unwrap();
        for directory in ["tmp", ".tmp", ".doty-quarantine"] {
            let path = root.path().join(directory);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("omitted"), b"omitted bytes").unwrap();
        }
        let usage = path_usage(root.path(), 20);
        assert!(!usage.complete);
        assert_eq!(usage.logical_bytes_lower_bound, 5);
        assert_eq!(usage.issues.len(), 3);
        assert!(
            usage
                .issues
                .iter()
                .all(|issue| issue.contains("excluded subtree"))
        );
    }
}
