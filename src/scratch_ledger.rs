//! Bounded Chaosbox contract. Read-only inspection and opt-in cleanup assessment.

use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Default, Args)]
pub struct Options {
    /// Private Chaosbox scratch custody directory (or CHAOSBOX_SCRATCH_WORK).
    #[arg(long, global = true)]
    chaosbox_work: Option<PathBuf>,
    /// Private visibility boundary (or CHAOSBOX_SCRATCH_SCOPE).
    #[arg(long, global = true)]
    chaosbox_scope: Option<String>,
    /// Host namespace (or CHAOSBOX_SCRATCH_HOST; defaults to local hostname).
    #[arg(long, global = true)]
    chaosbox_host: Option<String>,
    /// Allocation root (or CHAOSBOX_SCRATCH_ROOT).
    #[arg(long, global = true)]
    chaosbox_root: Option<PathBuf>,
    /// Chaosbox executable (or CHAOSBOX_SCRATCH_BIN; defaults to PATH).
    #[arg(long, global = true)]
    chaosbox_bin: Option<PathBuf>,
}

/// Pinned in removal plans, so resuming cannot silently drop ledger protection.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    binary: PathBuf,
    work: PathBuf,
    scope: String,
    host: String,
    root: PathBuf,
    #[serde(default)]
    assess: bool,
    #[serde(default = "default_requests")]
    max_requests: u32,
    #[serde(default = "default_tokens")]
    max_input_tokens: u64,
}

fn default_requests() -> u32 {
    1000
}
fn default_tokens() -> u64 {
    10_000_000
}

#[derive(Clone, Debug, Serialize)]
pub struct QueryPath {
    pub path: PathBuf,
    pub at: Option<PathBuf>,
}

impl Options {
    pub fn config(self) -> Result<Option<Config>> {
        let Some(work) = self
            .chaosbox_work
            .or_else(|| std::env::var_os("CHAOSBOX_SCRATCH_WORK").map(PathBuf::from))
        else {
            return Ok(None);
        };
        let scope = self
            .chaosbox_scope
            .or_else(|| std::env::var("CHAOSBOX_SCRATCH_SCOPE").ok())
            .context("Chaosbox scratch scope required")?;
        let host = self
            .chaosbox_host
            .or_else(|| std::env::var("CHAOSBOX_SCRATCH_HOST").ok())
            .or_else(|| {
                std::fs::read_to_string("/proc/sys/kernel/hostname")
                    .ok()
                    .map(|s| s.trim().to_owned())
            })
            .context("Chaosbox scratch host required")?;
        let root = self
            .chaosbox_root
            .or_else(|| std::env::var_os("CHAOSBOX_SCRATCH_ROOT").map(PathBuf::from))
            .unwrap_or_else(|| "/data/scratch/tmp/opencode".into());
        if !scope.starts_with("private:")
            || scope.len() <= 8
            || host.is_empty()
            || !work.is_absolute()
            || !root.is_absolute()
            || work.starts_with(&root)
            || root.starts_with(&work)
        {
            anyhow::bail!("invalid scratch ledger identity or state/root overlap");
        }
        let binary = self
            .chaosbox_bin
            .or_else(|| std::env::var_os("CHAOSBOX_SCRATCH_BIN").map(PathBuf::from))
            .unwrap_or_else(|| "chaosbox".into());
        Ok(Some(Config {
            binary,
            work,
            scope,
            host,
            root,
            assess: std::env::var("CHAOSBOX_SCRATCH_ASSESS").as_deref() == Ok("true"),
            max_requests: std::env::var("CHAOSBOX_SCRATCH_MAX_REQUESTS")
                .ok()
                .map(|s| s.parse())
                .transpose()
                .context("invalid scratch request budget")?
                .unwrap_or_else(default_requests),
            max_input_tokens: std::env::var("CHAOSBOX_SCRATCH_MAX_INPUT_TOKENS")
                .ok()
                .map(|s| s.parse())
                .transpose()
                .context("invalid scratch token budget")?
                .unwrap_or_else(default_tokens),
        }))
    }
}

impl Config {
    pub fn inspect(&self, paths: &[QueryPath]) -> Result<Value> {
        if paths.is_empty() {
            return Ok(
                serde_json::json!({"version":1,"scope":self.scope,"host":self.host,"root":self.root,"complete":true,"items":[]}),
            );
        }
        if paths.len() > 256 {
            anyhow::bail!("partition scratch queries into at most 256 paths");
        }
        self.validate(
            self.invoke(
                "query",
                serde_json::to_vec(paths)?,
                &[],
                Duration::from_secs(5),
            )?,
            paths,
        )
    }

    /// Refresh only stale, unreleased cleanup candidates. This explicit preparation
    /// path may use Jev; status/analyze/apply/purge continue to use read-only queries.
    pub fn inspect_for_cleanup(&self, paths: &[QueryPath]) -> Result<Value> {
        let report = self.inspect(paths)?;
        if !self.assess {
            return Ok(report);
        }
        let stale = stale_paths(&report);
        if stale.is_empty() {
            return Ok(report);
        }
        let mut args = vec![
            "--privacy-reviewed".to_owned(),
            "--max-requests".to_owned(),
            self.max_requests.to_string(),
            "--max-input-tokens".to_owned(),
            self.max_input_tokens.to_string(),
        ];
        for path in stale.iter().take(256) {
            args.push(path.clone());
        }
        let refreshed = self.invoke("assess", Vec::new(), &args, Duration::from_secs(60));
        let mut report = self.inspect(paths)?;
        if let Err(error) = refreshed {
            report["assessment_refresh_error"] = serde_json::json!(error.to_string());
        }
        Ok(report)
    }

    fn invoke(
        &self,
        action: &str,
        payload: Vec<u8>,
        args: &[String],
        timeout: Duration,
    ) -> Result<Value> {
        if payload.len() > MAX_BYTES as usize {
            anyhow::bail!("scratch query input budget exceeded");
        }
        let mut child = Command::new(&self.binary)
            .args(["scratch", "--work"])
            .arg(&self.work)
            .args(["--scope", &self.scope, "--host", &self.host, "--root"])
            .arg(&self.root)
            .arg(action)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot start Chaosbox scratch reader")?;
        let mut stdin = child.stdin.take().context("scratch reader stdin missing")?;
        let stdout = child
            .stdout
            .take()
            .context("scratch reader stdout missing")?;
        let stderr = child
            .stderr
            .take()
            .context("scratch reader stderr missing")?;
        let output = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        let errors = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.take(8192).read_to_end(&mut bytes).map(|_| bytes)
        });
        let input = std::thread::spawn(move || stdin.write_all(&payload));
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("Chaosbox scratch query timed out; cleanup intelligence unavailable");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let bytes = output
            .join()
            .map_err(|_| anyhow::anyhow!("scratch output reader failed"))??;
        input
            .join()
            .map_err(|_| anyhow::anyhow!("scratch input writer failed"))??;
        let errors = errors
            .join()
            .map_err(|_| anyhow::anyhow!("scratch error reader failed"))??;
        if !status.success() {
            anyhow::bail!(
                "Chaosbox scratch query failed: {}",
                serde_json::to_string(&String::from_utf8_lossy(&errors))?
            );
        }
        if bytes.len() > MAX_BYTES as usize {
            anyhow::bail!("scratch query response exceeds 4 MiB");
        }
        serde_json::from_slice(&bytes).context("invalid Chaosbox scratch JSON")
    }

    fn validate(&self, result: Value, paths: &[QueryPath]) -> Result<Value> {
        if result["version"] != 1
            || result["scope"] != self.scope
            || result["host"] != self.host
            || result["root"].as_str() != self.root.to_str()
            || !result["complete"].is_boolean()
        {
            anyhow::bail!("Chaosbox scratch response identity/schema mismatch");
        }
        let items = result["items"]
            .as_array()
            .context("scratch response items missing")?;
        if items.len() != paths.len() {
            anyhow::bail!("scratch response path coverage mismatch");
        }
        for (item, path) in items.iter().zip(paths) {
            if item["path"].as_str() != path.path.to_str()
                || !item["blocked"].is_boolean()
                || !item["reasons"].is_array()
                || !item["entries"].is_array()
                || item["at"].as_str() != path.at.as_ref().and_then(|p| p.to_str())
            {
                anyhow::bail!("scratch response allocation/schema mismatch");
            }
        }
        Ok(result)
    }

    pub fn require_released(&self, paths: &[QueryPath]) -> Result<Value> {
        let report = self.inspect(paths)?;
        ensure_released(&report)?;
        Ok(report)
    }

    pub fn validate_root(&self, root: &Path) -> Result<()> {
        if root != self.root {
            anyhow::bail!("removal root differs from pinned Chaosbox scratch root");
        }
        Ok(())
    }
}

fn stale_paths(report: &Value) -> Vec<String> {
    report["items"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| item["entries"].as_array().into_iter().flatten())
        .filter(|entry| {
            entry["disposition"] != "released" && entry["assessment_state"] != "current"
        })
        .filter_map(|entry| entry["path"].as_str().map(str::to_owned))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn source_label(source: &Value) -> String {
    if let Some(hash) = source["hash"].as_str() {
        format!("source {hash}")
    } else {
        format!("ledger annotation {}", source["annotation"])
    }
}

fn ensure_released(result: &Value) -> Result<()> {
    if result["complete"] != true {
        anyhow::bail!("scratch intelligence incomplete; finalization review required");
    }
    let items = result["items"]
        .as_array()
        .context("scratch allocations missing")?;
    for item in items {
        if item["blocked"] != false {
            anyhow::bail!(
                "scratch allocation {} needs review: {}",
                item["path"],
                item["reasons"]
            );
        }
    }
    Ok(())
}

pub fn paths(paths: &[PathBuf]) -> Vec<QueryPath> {
    paths
        .iter()
        .map(|path| QueryPath {
            path: path.clone(),
            at: None,
        })
        .collect()
}

pub fn print_report(report: &Value) {
    if let Some(error) = report.get("error") {
        println!("Scratch ledger unavailable: {error}");
    }
    if let Some(error) = report.get("assessment_refresh_error") {
        println!("Scratch assessment refresh pending: {error}");
    }
    for item in report["items"].as_array().into_iter().flatten() {
        println!(
            "  scratch: {} — {}",
            item["path"],
            if item["blocked"] == false {
                "released"
            } else {
                "finalization review"
            }
        );
        for reason in item["reasons"].as_array().into_iter().flatten() {
            println!("    {reason}");
        }
        for entry in item["entries"].as_array().into_iter().flatten() {
            if let Some(assessment) = entry.get("assessment").filter(|a| a.is_object()) {
                println!(
                    "    Jev-inferred recovery: priority {}, {}",
                    assessment["priority"],
                    if assessment["fresh"] == true {
                        "current evidence"
                    } else {
                        "stale evidence"
                    }
                );
                if let Some(quote) = assessment["purpose"]["quote"].as_str() {
                    println!(
                        "    purpose: {} ({})",
                        serde_json::json!(quote),
                        source_label(&assessment["purpose"])
                    );
                }
                for obligation in assessment["obligations"].as_array().into_iter().flatten() {
                    println!(
                        "    obligation [{}]: {} ({})",
                        obligation["status"],
                        obligation["source"]["quote"],
                        source_label(&obligation["source"])
                    );
                    if obligation["status"] != "satisfied" {
                        println!(
                            "    next: complete or reconcile this source-backed obligation before release"
                        );
                    }
                }
                if assessment["release_recommended"] == true && assessment["fresh"] == true {
                    println!("    release recommended; explicit release still required");
                }
                for source in assessment["unresolved_sources"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    println!(
                        "    unresolved purpose/work source: {} ({})",
                        source["quote"],
                        source_label(source)
                    );
                }
            } else {
                println!("    Jev assessment: {}", entry["assessment_state"]);
            }
            if let Some(annotation) = entry["annotations"].as_array().and_then(|a| a.last()) {
                println!("    purpose/work: {}", annotation["reason"]);
            }
            for invocation in entry["invocations"]
                .as_array()
                .into_iter()
                .flatten()
                .take(2)
            {
                println!(
                    "    session: {} command: {}",
                    invocation["session"], invocation["command"]
                );
                for source in invocation["sources"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(2)
                {
                    println!(
                        "    context: {} (source {})",
                        source["quote"], source["hash"]
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Config {
        Config {
            binary: "chaosbox".into(),
            work: "/state/custody".into(),
            scope: "private:test".into(),
            host: "test-host".into(),
            root: "/scratch".into(),
            assess: false,
            max_requests: default_requests(),
            max_input_tokens: default_tokens(),
        }
    }

    fn report() -> serde_json::Value {
        json!({"version":1,"scope":"private:test","host":"test-host","root":"/scratch","complete":true,
            "items":[{"path":"/scratch/work","blocked":false,"reasons":[],"entries":[]}]})
    }

    #[test]
    fn cleanup_refresh_selects_only_stale_unreleased_work_and_deduplicates_descendants() {
        let entries = json!([
            {"path":"/scratch/a","disposition":"open","assessment_state":"pending"},
            {"path":"/scratch/b","disposition":"released","assessment_state":"pending"},
            {"path":"/scratch/c","disposition":"needs-finalization","assessment_state":"current"}
        ]);
        let packet = json!({"items":[{"entries":entries},{"entries":entries}]});
        assert_eq!(stale_paths(&packet), vec!["/scratch/a"]);
    }

    #[test]
    fn contract_refuses_scope_host_path_and_schema_substitution() {
        let requested = vec![QueryPath {
            path: "/scratch/work".into(),
            at: None,
        }];
        for (field, value) in [
            ("host", json!("other-host")),
            ("scope", json!("private:other")),
            ("root", json!("/other-root")),
            ("version", json!(2)),
        ] {
            let mut result = report();
            result[field] = value;
            assert!(config().validate(result, &requested).is_err(), "{field}");
        }
        let mut result = report();
        result["items"][0]["path"] = json!("/scratch/work-other");
        assert!(config().validate(result, &requested).is_err());
    }

    #[test]
    fn incomplete_or_missing_intelligence_cannot_authorize_cleanup() {
        let requested = vec![QueryPath {
            path: "/scratch/work".into(),
            at: None,
        }];
        for field in ["complete", "items"] {
            let mut result = report();
            result.as_object_mut().unwrap().remove(field);
            assert!(config().validate(result, &requested).is_err());
        }
        let mut result = report();
        result["complete"] = json!(false);
        assert!(ensure_released(&result).is_err());
        let mut result = report();
        result["items"][0]["blocked"] = json!(true);
        result["items"][0]["reasons"] = json!(["nested patch needs finalization"]);
        assert!(
            ensure_released(&result)
                .unwrap_err()
                .to_string()
                .contains("nested patch")
        );
    }

    struct FakeChaosbox {
        temp: tempfile::TempDir,
        config: Config,
    }

    impl FakeChaosbox {
        fn new(initial: &Value, refreshed: &Value, assessment: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let temp = tempfile::tempdir().unwrap();
            let binary = temp.path().join("chaosbox");
            // Resolve the shell from the test environment, including Nix sandboxes.
            let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .map(|directory| directory.join("sh"))
                .find(|path| path.is_file())
                .unwrap()
                .canonicalize()
                .unwrap();
            std::fs::write(temp.path().join("initial.json"), initial.to_string()).unwrap();
            std::fs::write(temp.path().join("refreshed.json"), refreshed.to_string()).unwrap();
            std::fs::write(
                &binary,
                format!(
                    r#"#!{shell}
base=${{0%/*}}
printf '%s\n' CALL "$@" >> "$base/calls"
shift 9
case "$1" in
  query)
    cat > /dev/null
    if [ -f "$base/assessed" ]; then cat "$base/refreshed.json"; else cat "$base/initial.json"; fi
    ;;
  assess)
    : > "$base/assessed"
    {assessment}
    ;;
  *) exit 99 ;;
esac
"#,
                    shell = shell.display(),
                ),
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                temp,
                config: Config {
                    binary,
                    assess: true,
                    max_requests: 7,
                    max_input_tokens: 12345,
                    ..config()
                },
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            std::fs::read_to_string(self.temp.path().join("calls"))
                .unwrap()
                .split("CALL\n")
                .skip(1)
                .map(|call| call.lines().map(str::to_owned).collect())
                .collect()
        }
    }

    fn held_report(state: &str) -> Value {
        let mut packet = report();
        packet["items"][0]["blocked"] = json!(true);
        packet["items"][0]["reasons"] = json!(["work requires explicit release"]);
        packet["items"][0]["entries"] = json!([
            {"path":"/scratch/work","disposition":"needs-finalization","assessment_state":state}
        ]);
        packet
    }

    fn requested() -> Vec<QueryPath> {
        paths(&["/scratch/work".into()])
    }

    #[test]
    fn cleanup_refresh_executes_with_pinned_identity_budgets_and_requeries_holds() {
        let fake = FakeChaosbox::new(
            &held_report("pending"),
            &held_report("current"),
            "printf '%s' '{\"assessed\":1}'",
        );
        let result = fake.config.inspect_for_cleanup(&requested()).unwrap();
        assert_eq!(
            result["items"][0]["entries"][0]["assessment_state"],
            "current"
        );
        assert!(
            ensure_released(&result).is_err(),
            "assessment cannot release the hold"
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0][9], "query");
        assert_eq!(
            calls[1],
            vec![
                "scratch",
                "--work",
                "/state/custody",
                "--scope",
                "private:test",
                "--host",
                "test-host",
                "--root",
                "/scratch",
                "assess",
                "--privacy-reviewed",
                "--max-requests",
                "7",
                "--max-input-tokens",
                "12345",
                "/scratch/work",
            ]
        );
        assert_eq!(calls[2][9], "query");
    }

    #[test]
    fn ordinary_inspection_and_release_checks_never_dispatch_assessment() {
        let fake = FakeChaosbox::new(&held_report("pending"), &report(), "exit 99");
        let result = fake.config.inspect(&requested()).unwrap();
        assert_eq!(result["items"][0]["blocked"], true);
        assert!(fake.config.require_released(&requested()).is_err());
        assert!(fake.calls().iter().all(|call| call[9] == "query"));
        assert!(!fake.temp.path().join("assessed").exists());
    }

    #[test]
    fn disabled_or_current_or_released_cleanup_candidates_do_not_trigger_refresh() {
        for scenario in ["disabled", "current", "released"] {
            let mut initial = held_report("pending");
            if scenario == "current" {
                initial["items"][0]["entries"][0]["assessment_state"] = json!("current");
            } else if scenario == "released" {
                initial["items"][0]["entries"][0]["disposition"] = json!("released");
                initial["items"][0]["blocked"] = json!(false);
            }
            let mut fake = FakeChaosbox::new(&initial, &report(), "exit 99");
            fake.config.assess = scenario != "disabled";
            assert_eq!(
                fake.config.inspect_for_cleanup(&requested()).unwrap(),
                initial
            );
            assert_eq!(fake.calls().len(), 1, "{scenario}");
            assert!(!fake.temp.path().join("assessed").exists());
        }
    }

    #[test]
    fn refresh_failure_or_malformed_output_is_reported_and_latest_hold_is_retained() {
        for (body, expected) in [
            (
                "printf '%s' 'Jev unavailable' >&2; exit 23",
                "Jev unavailable",
            ),
            ("printf '%s' 'not-json'", "invalid Chaosbox scratch JSON"),
        ] {
            let mut refreshed = held_report("failed-or-interrupted");
            refreshed["items"][0]["reasons"] = json!(["new evidence requires finalization"]);
            let fake = FakeChaosbox::new(&held_report("pending"), &refreshed, body);
            let result = fake.config.inspect_for_cleanup(&requested()).unwrap();
            assert!(
                result["assessment_refresh_error"]
                    .as_str()
                    .unwrap()
                    .contains(expected)
            );
            assert_eq!(
                result["items"], refreshed["items"],
                "refresh failure still rereads custody"
            );
            assert!(ensure_released(&result).is_err());
            assert_eq!(fake.calls().len(), 3);
        }
    }

    #[test]
    fn a_substituted_post_refresh_identity_is_rejected() {
        let mut substituted = held_report("current");
        substituted["host"] = json!("other-host");
        let fake = FakeChaosbox::new(&held_report("pending"), &substituted, "printf '%s' '{}'");
        let error = fake.config.inspect_for_cleanup(&requested()).unwrap_err();
        assert!(error.to_string().contains("identity/schema mismatch"));
        assert_eq!(fake.calls().len(), 3);
    }

    #[test]
    fn timed_out_assessment_process_is_bounded_and_cannot_authorize_cleanup() {
        // exec keeps the sleeping process owned by invoke, without orphaned shell children.
        let fake = FakeChaosbox::new(
            &held_report("pending"),
            &held_report("pending"),
            "exec sleep 30",
        );
        let started = Instant::now();
        let error = fake
            .config
            .invoke("assess", Vec::new(), &[], Duration::from_millis(20))
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(fake.config.require_released(&requested()).is_err());
    }

    #[test]
    fn persisted_cleanup_configuration_keeps_assessment_policy_and_legacy_defaults() {
        let pinned = Config {
            assess: true,
            max_requests: 7,
            max_input_tokens: 12345,
            ..config()
        };
        let restored: Config =
            serde_json::from_value(serde_json::to_value(&pinned).unwrap()).unwrap();
        assert!(restored.assess);
        assert_eq!(restored.max_requests, 7);
        assert_eq!(restored.max_input_tokens, 12345);
        assert!(restored.validate_root(Path::new("/other-root")).is_err());
        let mut legacy = serde_json::to_value(&pinned).unwrap();
        for field in ["assess", "max_requests", "max_input_tokens"] {
            legacy.as_object_mut().unwrap().remove(field);
        }
        let restored: Config = serde_json::from_value(legacy).unwrap();
        assert!(!restored.assess);
        assert_eq!(restored.max_requests, default_requests());
        assert_eq!(restored.max_input_tokens, default_tokens());
    }
}
