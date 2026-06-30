use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/doty/targets.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredTarget {
    pub name: String,
    pub variant: String,
    #[serde(default)]
    pub settings: Value,
}

#[derive(Debug, Deserialize)]
struct TargetDocument {
    #[serde(default)]
    targets: Vec<ConfiguredTarget>,
}

pub fn load_targets(path: &str) -> Result<Vec<ConfiguredTarget>> {
    let content = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    parse_targets(&content).with_context(|| format!("cannot parse {path}"))
}

pub fn parse_targets(content: &str) -> Result<Vec<ConfiguredTarget>> {
    let parsed: TargetDocument = serde_json::from_str(content)?;
    Ok(parsed.targets)
}
