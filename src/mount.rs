use crate::exec;
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct MountInfo {
    pub mount_point: String,
    pub device: String,
    pub fstype: String,
    pub maj_min: String,
    pub fsroot: Option<String>,
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

#[derive(Debug, Deserialize)]
struct FindmntRoot {
    filesystems: Vec<FindmntEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct FindmntEntry {
    target: String,
    source: String,
    fstype: String,
    #[serde(rename = "maj:min")]
    maj_min: String,
    #[serde(default)]
    fsroot: Option<String>,
    #[serde(default)]
    children: Vec<FindmntEntry>,
}

struct MountIdentity {
    device: String,
    fstype: String,
    maj_min: String,
    fsroot: Option<String>,
}

fn findmnt_identities() -> Result<BTreeMap<String, MountIdentity>> {
    let out = exec::run_stdout(&[
        "findmnt", "--json", "--bytes",
        "--output", "TARGET,SOURCE,FSTYPE,MAJ:MIN,FSROOT",
    ])?;
    let root: FindmntRoot = serde_json::from_str(&out)?;
    let mut map = BTreeMap::new();
    flatten_findmnt(&root.filesystems, &mut map);
    Ok(map)
}

fn flatten_findmnt(
    entries: &[FindmntEntry],
    map: &mut BTreeMap<String, MountIdentity>,
) {
    for entry in entries {
        let device = clean_source(&entry.source);
        map.insert(entry.target.clone(), MountIdentity {
            device,
            fstype: entry.fstype.clone(),
            maj_min: entry.maj_min.clone(),
            fsroot: entry.fsroot.clone(),
        });
        flatten_findmnt(&entry.children, map);
    }
}

fn clean_source(source: &str) -> String {
    if let Some(end) = source.find('[') {
        source[..end].to_string()
    } else {
        source.to_string()
    }
}

fn should_exclude_fstype(fstype: &str) -> bool {
    matches!(fstype, "tmpfs" | "devtmpfs" | "devfs" | "overlay")
}

pub fn read_mounts() -> Result<Vec<MountInfo>> {
    let identities = findmnt_identities()?;

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

        if total_bytes == 0 || should_exclude_fstype(&fstype) {
            continue;
        }

        let identity = identities.get(mount_point);
        let device = identity.map_or_else(|| parts[0].to_string(), |i| i.device.clone());

        mounts.push(MountInfo {
            mount_point: mount_point.to_string(),
            device,
            fstype: identity.map_or_else(|| fstype.clone(), |i| i.fstype.clone()),
            maj_min: identity.map_or_else(String::new, |i| i.maj_min.clone()),
            fsroot: identity.and_then(|i| i.fsroot.clone()),
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
    let identities = findmnt_identities()?;

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
        let total_bytes = parts[2].parse::<u64>().unwrap_or(0);
        let used_bytes = parts[3].parse::<u64>().unwrap_or(0);
        let available_bytes = parts[4].parse::<u64>().unwrap_or(0);
        let fstype = parts[5].to_string();

        let identity = identities.get(mount);
        let device = identity.map_or_else(|| parts[0].to_string(), |i| i.device.clone());

        return Ok(MountInfo {
            mount_point: mount.to_string(),
            device,
            fstype: identity.map_or_else(|| fstype.clone(), |i| i.fstype.clone()),
            maj_min: identity.map_or_else(String::new, |i| i.maj_min.clone()),
            fsroot: identity.and_then(|i| i.fsroot.clone()),
            total_bytes,
            used_bytes,
            available_bytes,
        });
    }

    anyhow::bail!("mount point not found: {mount}")
}
