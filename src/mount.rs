use crate::exec;
use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct MountInfo {
    pub mount_point: String,
    pub device: String,
    pub fstype: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

impl MountInfo {
    pub fn usage_pct(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes as f64 / self.total_bytes as f64) * 100.0
    }
}

pub fn read_mounts() -> Result<Vec<MountInfo>> {
    let out = exec::run_stdout(&[
        "df",
        "--exclude-type=tmpfs",
        "--exclude-type=devtmpfs",
        "--exclude-type=devfs",
        "--exclude-type=overlay",
        "-B1",
        "--output=source,target,size,used,avail,fstype",
    ])?;

    let mut mounts: Vec<MountInfo> = Vec::new();
    for line in out.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            continue;
        }

        let device = parts[0].to_string();
        let mount_point = parts[1].trim_end_matches('/');
        let mount_point = if mount_point.is_empty() {
            "/"
        } else {
            mount_point
        };
        let total_bytes = parts[2].parse::<u64>().unwrap_or(0);
        let used_bytes = parts[3].parse::<u64>().unwrap_or(0);
        let available_bytes = parts[4].parse::<u64>().unwrap_or(0);
        let fstype = parts[5].to_string();

        if total_bytes == 0 {
            continue;
        }

        mounts.push(MountInfo {
            mount_point: mount_point.to_string(),
            device,
            fstype,
            total_bytes,
            used_bytes,
            available_bytes,
        });
    }

    mounts.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    Ok(mounts)
}

pub fn mount_for_path(path: &str) -> Option<String> {
    let path = Path::new(path);
    let path_str = path.to_string_lossy();

    let mounts = read_mounts().ok()?;
    let mut best_match: Option<String> = None;
    let mut best_len = 0usize;

    for m in &mounts {
        let mp = &m.mount_point;
        if mp == "/" {
            continue;
        }
        if path_str.starts_with(mp) && mp.len() > best_len {
            best_len = mp.len();
            best_match = Some(mp.clone());
        }
    }

    if let Some(mp) = &best_match {
        return Some(mp.clone());
    }

    if path_str.starts_with("/") {
        return Some("/".to_string());
    }

    None
}

pub fn df(mount: &str) -> Result<MountInfo> {
    let mount = mount.trim_end_matches('/');
    let mount = if mount.is_empty() { "/" } else { mount };
    let out = exec::run_stdout(&[
        "df",
        "--exclude-type=tmpfs",
        "--exclude-type=devtmpfs",
        "-B1",
        "--output=source,target,size,used,avail,fstype",
        mount,
    ])?;

    for line in out.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            continue;
        }
        let device = parts[0].to_string();
        let total_bytes = parts[2].parse::<u64>().unwrap_or(0);
        let used_bytes = parts[3].parse::<u64>().unwrap_or(0);
        let available_bytes = parts[4].parse::<u64>().unwrap_or(0);
        let fstype = parts[5].to_string();

        return Ok(MountInfo {
            mount_point: mount.to_string(),
            device,
            fstype,
            total_bytes,
            used_bytes,
            available_bytes,
        });
    }

    anyhow::bail!("mount point not found: {mount}")
}
