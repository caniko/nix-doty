use crate::exec;
use crate::framework::{ApplyReport, Inspection, Variant};
use serde_json::Value;

pub(crate) fn paths_from_settings(settings: &Value, defaults: &[&str]) -> Vec<String> {
    let paths: Vec<String> = settings
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect();
    if paths.is_empty() {
        defaults.iter().map(|p| (*p).to_string()).collect()
    } else {
        paths
    }
}

pub(crate) fn report_only_apply(v: &dyn Variant) -> ApplyReport {
    ApplyReport {
        framework: v.framework().name(),
        variant: v.name(),
        removed: 0,
        freed_bytes: 0,
        skipped: 0,
        errors: vec![],
    }
}

pub(crate) fn inspect_paths(
    framework: &'static str,
    variant: &'static str,
    defaults: &[&str],
    settings: &Value,
    summary: &str,
) -> Inspection {
    let paths = paths_from_settings(settings, defaults);
    let existing: Vec<String> = paths
        .iter()
        .filter(|p| exec::path_exists(p))
        .cloned()
        .collect();
    let scan = exec::total_paths_size_bounded(&existing, 20_000);
    let path = if existing.is_empty() {
        paths.join(", ")
    } else {
        existing.join(", ")
    };
    Inspection {
        framework,
        variant,
        path,
        size_bytes: Some(scan.bytes),
        age_oldest_days: None,
        would_remove: 0,
        notes: format!(
            "{summary}: {} configured paths, {} present{}",
            paths.len(),
            existing.len(),
            if scan.truncated {
                " (scan truncated)"
            } else {
                ""
            }
        ),
    }
}
