//! Persistent build/service storage inspection. Application stores own their GC.
use super::*;

struct StorageFramework {
    name: &'static str,
    summary: &'static str,
    variants: &'static [&'static dyn Variant],
}
impl Framework for StorageFramework {
    fn name(&self) -> &'static str {
        self.name
    }
    fn summary(&self) -> &'static str {
        self.summary
    }
    fn variants(&self) -> &[&'static dyn Variant] {
        self.variants
    }
}
static BUILDS: StorageFramework = StorageFramework {
    name: "nix-builds",
    summary: "Persistent Nix build directories with PID and open-reference evidence",
    variants: &[&BUILD_REPORT],
};
static SERVICES: StorageFramework = StorageFramework {
    name: "service-storage",
    summary: "Current service storage and application-owned retention policy",
    variants: &[&SERVICE_REPORT],
};
pub static NIX_BUILDS: &dyn Framework = &BUILDS;
pub static SERVICE_STORAGE: &dyn Framework = &SERVICES;
struct StorageReport {
    builds: bool,
}
static BUILD_REPORT: StorageReport = StorageReport { builds: true };
static SERVICE_REPORT: StorageReport = StorageReport { builds: false };

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct StorageSettings {
    roots: Vec<PathBuf>,
    paths: Vec<String>,
    retention_owner: Option<String>,
    retention_policy: Option<String>,
}

fn build_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("nix-")?;
    let (pid, suffix) = rest.split_once('-')?;
    let pid = pid.parse().ok()?;
    (!suffix.is_empty() && pid > 0).then_some(pid)
}

impl Variant for StorageReport {
    fn name(&self) -> &'static str {
        if self.builds {
            "abandoned-report"
        } else {
            "disk-report"
        }
    }
    fn framework(&self) -> &'static dyn Framework {
        if self.builds { &BUILDS } else { &SERVICES }
    }
    fn tier(&self) -> Tier {
        Tier::ReportOnly
    }
    fn inspect(&self) -> Result<Inspection> {
        self.inspect_with_settings(&serde_json::json!({}))
    }
    fn apply(&self, _: bool, _: bool) -> Result<ApplyReport> {
        Ok(crate::targets::report::report_only_apply(self))
    }
    fn apply_with_settings(&self, _: bool, _: bool, _: &Value) -> Result<ApplyReport> {
        self.apply(false, false)
    }
    fn inspect_with_settings(&self, value: &Value) -> Result<Inspection> {
        let settings: StorageSettings = serde_json::from_value(value.clone())?;
        if !self.builds {
            let mut report = crate::targets::report::inspect_paths(
                self.framework().name(),
                self.name(),
                &[],
                value,
                "Application-owned GC; footprints are not reclaim estimates",
            );
            let mut notes: Value = serde_json::from_str(&report.notes)?;
            notes["retentionOwner"] = serde_json::to_value(settings.retention_owner)?;
            notes["retentionPolicy"] = serde_json::to_value(settings.retention_policy)?;
            report.notes = notes.to_string();
            return Ok(report);
        }
        let mut entries = Vec::new();
        let mut issues = Vec::new();
        let mut processes = BTreeMap::new();
        let root_operator = fs::metadata("/proc/self")?.uid() == 0;
        let mut complete = true;
        for root in &settings.roots {
            let mut budget = 100_000;
            let discovered = real_directory(root).and_then(|()| children(root, &mut budget));
            match discovered {
                Err(error) => issues.push(format!("{}: {error}", root.display())),
                Ok(paths) => {
                    for path in paths {
                        let Some(pid) = build_pid(&path) else {
                            continue;
                        };
                        match fs::symlink_metadata(&path) {
                            Ok(metadata) if metadata.is_dir() => (),
                            Ok(_) => continue,
                            Err(error) => {
                                issues.push(format!("{}: {error}", path.display()));
                                continue;
                            }
                        }
                        let usage = crate::targets::report::path_usage(&path, 100_000);
                        let owner = fs::symlink_metadata(&path).map(|m| m.uid());
                        let process_use = owner.map_err(|error| error.to_string()).map(|uid| {
                            // A root scan already inspects every process owner.
                            processes
                                .entry(if root_operator { 0 } else { uid })
                                .or_insert_with(|| {
                                    process_paths(uid).map_err(|error| error.to_string())
                                })
                        });
                        let pid_absent = fs::symlink_metadata(format!("/proc/{pid}"))
                            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
                        let (referenced, observable, process_issues) = match process_use {
                            Ok(Ok(p)) => (p.references(&path), !p.unknown, p.issues.clone()),
                            Ok(Err(e)) => (false, false, vec![e.clone()]),
                            Err(e) => (false, false, vec![e]),
                        };
                        complete &= usage.complete && observable;
                        entries.push(serde_json::json!({"path": path, "pid": pid,
                        "pidAbsent": pid_absent, "referenced": referenced,
                        "processInspectionComplete": observable, "processIssues": process_issues,
                        "abandonedCandidate": pid_absent && !referenced && observable && usage.complete,
                        "eligible": false, "usage": usage,
                        "blocker": "PID absence does not authorize removal; Nix owns live build lifetimes"}));
                    }
                }
            }
        }
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: settings
                .roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            size_bytes: None,
            age_oldest_days: None,
            would_remove: 0,
            notes: serde_json::json!({"schemaVersion": 1, "complete": complete && issues.is_empty(), "entries": entries, "issues": issues}).to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pid_shape_is_discovery_evidence_not_deletion_authority() {
        assert_eq!(build_pid(Path::new("/build/nix-1234-abcd")), Some(1234));
        for name in ["nix-0-abcd", "nix-1234-", "tmp", "nix-unknown-abcd"] {
            assert_eq!(build_pid(Path::new(name)), None);
        }
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("nix-4294967295-abcd")).unwrap();
        fs::write(
            root.path().join("nix-4294967295-file"),
            b"not a build directory",
        )
        .unwrap();
        let report = BUILD_REPORT
            .inspect_with_settings(&serde_json::json!({"roots": [root.path()]}))
            .unwrap();
        let notes: Value = serde_json::from_str(&report.notes).unwrap();
        assert_eq!(notes["entries"].as_array().unwrap().len(), 1);
        assert_eq!(notes["entries"][0]["pidAbsent"], true);
        assert_eq!(notes["entries"][0]["eligible"], false);
        assert_eq!(BUILD_REPORT.apply(true, true).unwrap().removed, 0);
    }

    #[test]
    fn missing_build_root_does_not_hide_independent_siblings() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("nix-4294967295-abcd")).unwrap();
        let report = BUILD_REPORT
            .inspect_with_settings(
                &serde_json::json!({"roots": [root.path().join("missing"), root.path()]}),
            )
            .unwrap();
        let notes: Value = serde_json::from_str(&report.notes).unwrap();
        assert_eq!(notes["complete"], false);
        assert_eq!(notes["entries"].as_array().unwrap().len(), 1);
        assert!(notes["issues"].to_string().contains("missing"));
    }
}
