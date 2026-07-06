use crate::framework::{Framework, Variant};

pub static ALL_FRAMEWORKS: &[&dyn Framework] = &[
    crate::targets::NIX_STORE_GC,
    crate::targets::BOOT_GENERATIONS,
    crate::targets::BUILD_MEMORY_LOGS,
    crate::targets::CANIX_REBUILD_LOGS,
    crate::targets::TMP_STALE,
    crate::targets::JOURNALD_VACUUM,
    crate::targets::FAILED_UNITS,
    crate::targets::CGROUP_RESET,
    crate::targets::LLAMA_MODELS,
    crate::targets::COMFYUI_STATE,
    crate::targets::UV_CACHE,
    crate::targets::DNS_RUNTIME_CACHE,
    crate::targets::FREEDESKTOP_TRASH,
    crate::targets::QBITTORRENT_TORRENTS,
    crate::targets::PINK_RAVEN_WORKERS,
    crate::targets::IMMICH_TEMPORAL,
    crate::targets::BTRFS_SNAPSHOTS,
    crate::targets::STEAMPIPE_STATE,
    crate::targets::CHESSBENDER_STATE,
    crate::targets::OPENCODE_CACHE,
    crate::targets::CANIX_PREFLIGHT_CACHE,
    crate::targets::USER_CACHE,
    crate::targets::FORGEJO_RUNNER_CACHE,
    crate::targets::SCCACHE_GARAGE,
    crate::targets::MEDIA_STACK_STATE,
    crate::targets::FOUNDRY_VTT_STATE,
    crate::targets::OPENVSCODE_STATE,
    crate::targets::PG_BACKUP_STATE,
    crate::targets::PODMAN_IMAGES,
];

pub fn find_framework(name: &str) -> Option<&'static dyn Framework> {
    ALL_FRAMEWORKS.iter().copied().find(|f| f.name() == name)
}

pub fn find_variant(framework_name: &str, variant_name: &str) -> Option<&'static dyn Variant> {
    let framework = find_framework(framework_name)?;
    framework
        .variants()
        .iter()
        .copied()
        .find(|v| v.name() == variant_name)
}
