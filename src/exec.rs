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
    // Never remove through a symlink or special file: callers pass cache
    // entries, not links. Full policy checks live in guard::guard_path.
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("failed to stat {path}"))?;
    if meta.file_type().is_symlink() {
        anyhow::bail!("refusing to remove symlink: {path}");
    }
    if !meta.is_file() {
        anyhow::bail!("refusing to remove non-file: {path}");
    }
    std::fs::remove_file(path).with_context(|| format!("failed to remove {path}"))
}

pub fn remove_dir_all(path: &str) -> Result<()> {
    // Same as remove_file: refuse symlinks and non-directories up front.
    // Contents are the caller's responsibility; prefer guard::guard_path
    // plus the rm quarantine flow for anything not strictly cache-shaped.
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("failed to stat {path}"))?;
    if meta.file_type().is_symlink() {
        anyhow::bail!("refusing to remove symlink: {path}");
    }
    if !meta.is_dir() {
        anyhow::bail!("refusing to remove non-directory: {path}");
    }
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
        // Never follow or delete symlinks and special files: age-based
        // pruning must not traverse somewhere the mtime did not vouch for.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if !file_type.is_dir() && !file_type.is_file() {
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_file_refuses_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "data").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = remove_file(link.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(real.exists());
    }

    #[test]
    fn remove_dir_all_refuses_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("inner.txt"), "data").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = remove_dir_all(link.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(real.join("inner.txt").exists());
    }

    #[test]
    fn remove_stale_entries_skips_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let victim = outside.path().join("keep.txt");
        std::fs::write(&victim, "data").unwrap();
        // Backdate everything well past the cutoff.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86400);
        std::os::unix::fs::symlink(&victim, dir.path().join("link.txt")).unwrap();
        set_mtime(&dir.path().join("link.txt"), old);
        let stale = dir.path().join("stale.txt");
        std::fs::write(&stale, "old").unwrap();
        set_mtime(&stale, old);
        set_mtime(dir.path(), old);
        let (removed, _) = remove_stale_entries(dir.path().to_str().unwrap(), 30, 2).unwrap();
        assert!(victim.exists());
        assert!(!stale.exists());
        assert_eq!(removed, 1);
    }

    fn set_mtime(path: &Path, modified: std::time::SystemTime) {
        // filetime-equivalent via libc utimensat through std: fall back to
        // touching with a subprocess-free approach is unavailable, so use
        // the `touch -d` helper present on this host.
        let secs = modified
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let _ = std::process::Command::new("touch")
            .args(["-d", &format!("@{secs}"), &path.to_string_lossy()])
            .status();
    }
}
