use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, Default)]
pub struct DirScan {
    pub bytes: u64,
    pub entries: u64,
    pub truncated: bool,
}

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
        anyhow::bail!(
            "command `{cmd:?}` failed (exit {}):\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code().unwrap_or(-1)
        );
    }
    Ok(stdout)
}

pub fn run_stdout(cmd: &[&str]) -> Result<String> {
    run(cmd)
}

pub fn read_file(path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("failed to read {path}"))
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
    std::fs::remove_file(path).with_context(|| format!("failed to remove {path}"))
}

pub fn remove_dir_all(path: &str) -> Result<()> {
    std::fs::remove_dir_all(path).with_context(|| format!("failed to remove directory {path}"))
}

pub fn file_size(path: &str) -> Result<u64> {
    let meta = std::fs::metadata(path).with_context(|| format!("failed to stat {path}"))?;
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

pub fn total_dir_size_bounded(path: &str, max_entries: u64) -> Result<DirScan> {
    let mut scan = DirScan::default();
    fn visit(dir: &Path, max_entries: u64, scan: &mut DirScan) -> Result<()> {
        if scan.entries >= max_entries {
            scan.truncated = true;
            return Ok(());
        }
        if dir.is_dir() {
            for entry in std::fs::read_dir(dir)? {
                if scan.entries >= max_entries {
                    scan.truncated = true;
                    break;
                }
                let entry = entry?;
                let path = entry.path();
                scan.entries += 1;
                if path.is_dir() {
                    visit(&path, max_entries, scan)?;
                } else {
                    scan.bytes = scan.bytes.saturating_add(entry.metadata()?.len());
                }
            }
        }
        Ok(())
    }
    visit(Path::new(path), max_entries, &mut scan)?;
    Ok(scan)
}

pub fn total_paths_size_bounded(paths: &[String], max_entries_per_path: u64) -> DirScan {
    let mut total = DirScan::default();
    for path in paths {
        match total_dir_size_bounded(path, max_entries_per_path) {
            Ok(scan) => {
                total.bytes = total.bytes.saturating_add(scan.bytes);
                total.entries = total.entries.saturating_add(scan.entries);
                total.truncated |= scan.truncated;
            }
            Err(_) => {
                total.truncated = true;
            }
        }
    }
    total
}

pub fn dir_entry_count(path: &str) -> Result<u64> {
    let count = std::fs::read_dir(path)?.filter_map(|e| e.ok()).count() as u64;
    Ok(count)
}

pub fn all_user_subdirs(subpath: &str) -> Vec<String> {
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/home") {
        for entry in entries.flatten() {
            let user = entry.file_name();
            let path = format!("/home/{}/{}", user.to_string_lossy(), subpath);
            if Path::new(&path).exists() {
                dirs.push(path);
            }
        }
    }
    let root_path = format!("/root/{subpath}");
    if Path::new(&root_path).exists() {
        dirs.push(root_path);
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

pub fn remove_stale_entries(dir: &str, max_age_days: u32, max_depth: u32) -> Result<(u64, u64)> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(max_age_days as i64);
    let mut removed: u64 = 0;
    let mut freed: u64 = 0;
    remove_stale_inner(
        Path::new(dir),
        &cutoff,
        max_depth,
        0,
        &mut removed,
        &mut freed,
    )
    .map(|_| (removed, freed))
}

fn remove_stale_inner(
    dir: &Path,
    cutoff: &chrono::DateTime<chrono::Utc>,
    max_depth: u32,
    depth: u32,
    removed: &mut u64,
    freed: &mut u64,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let age: chrono::DateTime<chrono::Utc> = modified.into();
        if age >= *cutoff {
            continue;
        }
        if metadata.is_dir() {
            if depth < max_depth {
                remove_stale_inner(&path, cutoff, max_depth, depth + 1, removed, freed)?;
            }
            if let Ok(size) = total_dir_size(&path.to_string_lossy()) {
                *freed += size;
            }
            let _ = std::fs::remove_dir_all(&path);
            *removed += 1;
        } else {
            *freed += metadata.len();
            let _ = std::fs::remove_file(&path);
            *removed += 1;
        }
    }
    Ok(())
}
