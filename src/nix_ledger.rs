//! Read-only Nix provenance from Chaosbox. A reason is evidence for review,
//! never authorization to delete a shared store object or another workflow's root.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub fn inspect(path: Option<&str>, offset: usize) -> Result<Value> {
    let required =
        |key: &str| std::env::var(key).with_context(|| format!("operator setting {key} required"));
    let binary = required("DOTY_NIX_CHAOSBOX_BIN")?;
    let scope = required("DOTY_NIX_SCOPE")?;
    let host = required("DOTY_NIX_HOST")?;
    let store = std::env::var("DOTY_NIX_STORE").unwrap_or_else(|_| "daemon".into());
    if !Path::new(&binary).is_absolute()
        || !scope.starts_with("private:")
        || host.trim().is_empty()
        || offset > 1_000_000
    {
        anyhow::bail!("invalid Nix provenance reader configuration/bounds");
    }
    let mut command = Command::new(&binary);
    command.args([
        "nix",
        "query",
        "--limit",
        "20",
        "--offset",
        &offset.to_string(),
    ]);
    if let Some(path) = path {
        command.args(["--path", path]);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start Chaosbox Nix reader")?;
    let stdout = child.stdout.take().context("Nix reader stdout missing")?;
    let stderr = child.stderr.take().context("Nix reader stderr missing")?;
    let out = std::thread::spawn(move || bounded_read(stdout, MAX_BYTES));
    let errors = std::thread::spawn(move || bounded_read(stderr, 8192));
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Chaosbox Nix query timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = out
        .join()
        .map_err(|_| anyhow::anyhow!("Nix output reader failed"))??;
    let error = errors
        .join()
        .map_err(|_| anyhow::anyhow!("Nix diagnostic reader failed"))??;
    if !status.success() {
        anyhow::bail!(
            "Chaosbox Nix query failed: {}",
            serde_json::to_string(&String::from_utf8_lossy(&error))?
        );
    }
    let result: Value = serde_json::from_slice(&bytes).context("invalid Chaosbox Nix JSON")?;
    validate(&result, &scope, &host, &store)?;
    Ok(result)
}

fn bounded_read(mut reader: impl Read, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    // Drain over-budget output so a writer cannot deadlock on its stdout pipe.
    std::io::copy(&mut reader, &mut std::io::sink())?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other("Nix reader output budget exceeded"));
    }
    Ok(bytes)
}

fn validate(result: &Value, scope: &str, host: &str, store: &str) -> Result<()> {
    if result["version"] != 1
        || result["scope"] != scope
        || result["host"] != host
        || result["store"] != store
        || result["cleanup_authorized"] != false
        || !result["complete"].is_boolean()
    {
        anyhow::bail!("Chaosbox Nix response namespace/version mismatch");
    }
    let items = result["items"]
        .as_array()
        .context("Nix query items required")?;
    if items.len() > 20 {
        anyhow::bail!("Nix query item budget exceeded");
    }
    for item in items {
        if item["id"].as_str().is_none()
            || item["reason"].as_str().is_none_or(|r| r.trim().is_empty())
            || item["retention"] != "unrooted"
            || item["owned_roots"]
                .as_array()
                .is_none_or(|roots| !roots.is_empty())
            || item["disposition"] != "needs-review"
            || item["gc_eligibility"] != "unknown"
        {
            anyhow::bail!("unsupported Nix cleanup obligation contract");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_foreign_or_mutating_cleanup_packets() {
        let packet = json!({"version":1,"scope":"private:can","host":"atlas","store":"daemon","cleanup_authorized":false,"complete":true,"items":[]});
        assert!(validate(&packet, "private:can", "atlas", "daemon").is_ok());
        for (key, value) in [
            ("version", json!(2)),
            ("scope", json!("private:other")),
            ("host", json!("nomad")),
            ("cleanup_authorized", json!(true)),
        ] {
            let mut other = packet.clone();
            other[key] = value;
            assert!(validate(&other, "private:can", "atlas", "daemon").is_err());
        }
        let mut owned = packet;
        owned["items"] = json!([{"id":"one","reason":"needed","retention":"held","owned_roots":["/root"],"disposition":"needs-review","gc_eligibility":"unknown"}]);
        assert!(validate(&owned, "private:can", "atlas", "daemon").is_err());
    }
}
