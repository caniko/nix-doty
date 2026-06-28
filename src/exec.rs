use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub fn run(cmd: &[&str]) -> Result<String> {
    if cmd.is_empty() {
        anyhow::bail!("exec::run called with empty command");
    }
    let program = cmd[0];
    let args = &cmd[1..];
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {program}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        anyhow::bail!("command `{cmd:?}` failed (exit {}):\nstdout: {stdout}\nstderr: {stderr}", output.status.code().unwrap_or(-1));
    }
    Ok(stdout)
}

pub fn run_stdout(cmd: &[&str]) -> Result<String> {
    run(cmd)
}

pub fn read_file(path: &str) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {path}"))
}

pub fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

pub fn read_dir(path: &str) -> Result<Vec<String>> {
    let mut entries: Vec<String> = std::fs::read_dir(path)
        .with_context(|| format!("failed to read directory {path}"))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .map(|s| format!("{}/{}", path.trim_end_matches('/'), s))
        })
        .collect();
    entries.sort();
    Ok(entries)
}

pub fn remove_file(path: &str) -> Result<()> {
    std::fs::remove_file(path)
        .with_context(|| format!("failed to remove {path}"))
}

pub fn remove_dir_all(path: &str) -> Result<()> {
    std::fs::remove_dir_all(path)
        .with_context(|| format!("failed to remove directory {path}"))
}

pub fn file_size(path: &str) -> Result<u64> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {path}"))?;
    Ok(meta.len())
}

pub fn total_dir_size(path: &str) -> Result<u64> {
    let mut total = 0u64;
    fn visit(dir: &Path, total: &mut u64) -> Result<()> {
        if dir.is_dir() {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, total)?;
                } else {
                    *total += entry.metadata()?.len();
                }
            }
        }
        Ok(())
    }
    visit(Path::new(path), &mut total)?;
    Ok(total)
}

pub fn dir_entry_count(path: &str) -> Result<u64> {
    let count = std::fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .count() as u64;
    Ok(count)
}
