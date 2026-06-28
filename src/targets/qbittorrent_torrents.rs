use anyhow::Result;
use crate::exec;
use crate::framework::{ApplyReport, Framework, Inspection, Tier, Variant};

struct QbittorrentTorrentsFramework;

impl Framework for QbittorrentTorrentsFramework {
    fn name(&self) -> &'static str { "qbittorrent-torrents" }
    fn summary(&self) -> &'static str { "qBittorrent stale torrents (report only)" }
    fn variants(&self) -> &[&'static dyn Variant] {
        &[&StaleReport]
    }
}

static FRAMEWORK: QbittorrentTorrentsFramework = QbittorrentTorrentsFramework;

pub static QBITTORRENT_TORRENTS: &dyn Framework = &FRAMEWORK;

fn data_dirs() -> Vec<String> {
    let paths = ["/data/nvme0/downloads/tv-shows", "/data/nvme0/downloads/movies"];
    paths.iter()
        .filter(|p| exec::path_exists(p))
        .flat_map(|p| {
            let entries = exec::read_dir(p).unwrap_or_default();
            entries.into_iter().map(move |e| format!("{p}/{e}"))
        })
        .collect()
}

struct StaleReport;
impl Variant for StaleReport {
    fn name(&self) -> &'static str { "stale-report" }
    fn framework(&self) -> &'static dyn Framework { &FRAMEWORK }
    fn tier(&self) -> Tier { Tier::ReportOnly }
    fn inspect(&self) -> Result<Inspection> {
        let dirs = data_dirs();
        let count = dirs.len() as u64;
        let total_size: u64 = dirs.iter()
            .filter_map(|d| exec::file_size(d).ok())
            .sum();
        Ok(Inspection {
            framework: self.framework().name(),
            variant: self.name(),
            path: "/data/nvme0/downloads".into(),
            size_bytes: Some(total_size),
            age_oldest_days: None,
            would_remove: 0,
            notes: format!("{count} download directories — report only"),
        })
    }
    fn apply(&self, _dry_run: bool, _force: bool) -> Result<ApplyReport> {
        Ok(ApplyReport {
            framework: self.framework().name(),
            variant: self.name(),
            removed: 0, freed_bytes: 0, skipped: 0, errors: vec![],
        })
    }
}
