//! Read-only Nix provenance from Chaosbox. A reason is evidence for review,
//! never authorization to delete a shared store object or another workflow's root.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
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
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start Chaosbox Nix reader")?;
    let (status, bytes, error) = read_query_output(child, Duration::from_secs(5))?;
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

fn nonblocking(reader: &impl AsRawFd) -> io::Result<()> {
    // These owned pipe descriptors remain live for both fcntl calls.
    let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_available(reader: &mut impl Read, bytes: &mut Vec<u8>, limit: u64) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    // Bound each drain, so a continuously writing descendant cannot starve the deadline.
    for _ in 0..8 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                if bytes.len() as u64 + count as u64 > limit {
                    return Err(io::Error::other("Nix reader output budget exceeded"));
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn read_query_output(
    mut child: Child,
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let result = (|| {
        let mut stdout = child.stdout.take().context("Nix reader stdout missing")?;
        let mut stderr = child.stderr.take().context("Nix reader stderr missing")?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let start = Instant::now();
        let mut status = None;
        let (mut out, mut errors) = (Vec::new(), Vec::new());
        let (mut out_closed, mut errors_closed) = (false, false);
        loop {
            if start.elapsed() >= timeout {
                anyhow::bail!("Chaosbox Nix query timed out");
            }
            if !out_closed {
                out_closed = read_available(&mut stdout, &mut out, MAX_BYTES)?;
            }
            if !errors_closed {
                errors_closed = read_available(&mut stderr, &mut errors, 8192)?;
            }
            if status.is_none() {
                status = child.try_wait()?;
            }
            if out_closed && errors_closed {
                if let Some(status) = status {
                    return Ok((status, out, errors));
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
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
            || !matches!(
                item["outcome"].as_str(),
                Some("succeeded" | "failed" | "interrupted" | "not-started" | "unresolved")
            )
            || !matches!(
                item["filesystem_presence"].as_str(),
                Some("present" | "absent" | "unknown")
            )
            || item["registered_validity"] != "unknown"
            || !item["objects"].is_array()
            || !item["settled"].is_boolean()
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
    fn an_exited_reader_with_inherited_open_pipes_still_obeys_the_deadline() {
        let child = Command::new("sh")
            .args(["-c", "sleep 1 >&1 2>&2 & exit 0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let error = read_query_output(child, Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_millis(800));
    }

    #[test]
    fn execution_and_filesystem_state_are_required_even_for_unresolved_items() {
        let item = json!({"id":"one","reason":"needed","retention":"unrooted","owned_roots":[],"disposition":"needs-review","gc_eligibility":"unknown","outcome":"unresolved","filesystem_presence":"unknown","registered_validity":"unknown","objects":[],"settled":false});
        let packet = json!({"version":1,"scope":"private:can","host":"atlas","store":"daemon","cleanup_authorized":false,"complete":true,"items":[item]});
        assert!(validate(&packet, "private:can", "atlas", "daemon").is_ok());
        for field in [
            "outcome",
            "filesystem_presence",
            "registered_validity",
            "objects",
            "settled",
        ] {
            let mut incomplete = packet.clone();
            incomplete["items"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                validate(&incomplete, "private:can", "atlas", "daemon").is_err(),
                "{field}"
            );
        }
    }

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
