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
pub struct TargetDocument {
    /// Unmarked legacy documents are system policy.
    #[serde(default)]
    pub mode: Option<crate::scope::Mode>,
    #[serde(default)]
    pub targets: Vec<ConfiguredTarget>,
    #[serde(default)]
    pub requirements: Vec<Requirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Requirement {
    pub name: String,
    pub variant: String,
    pub path: String,
}

pub fn load_document(path: &str) -> Result<TargetDocument> {
    let content = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    serde_json::from_str(&content).with_context(|| format!("cannot parse {path}"))
}
