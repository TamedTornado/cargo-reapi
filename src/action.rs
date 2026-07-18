use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Serialize)]
pub struct DeterministicAction {
    pub compiler: ToolchainIdentity,
    pub platform: PlatformIdentity,
    pub working_directory: String,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub inputs: Vec<ActionInput>,
    pub outputs: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ToolchainIdentity {
    pub sha256: String,
    pub size_bytes: u64,
    pub version: String,
}

#[derive(Debug, Serialize)]
pub struct PlatformIdentity {
    pub os: &'static str,
    pub arch: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ActionInput {
    pub path: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct RemoteEligibility {
    pub eligible: bool,
    pub reasons: Vec<String>,
}

impl RemoteEligibility {
    pub fn from_reasons(mut reasons: Vec<String>) -> Self {
        reasons.sort();
        reasons.dedup();
        Self {
            eligible: reasons.is_empty(),
            reasons,
        }
    }
}

pub fn action_key(action: &DeterministicAction) -> Result<String> {
    let encoded = serde_json::to_vec(action).context("serializing deterministic action")?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(arguments: &[&str]) -> DeterministicAction {
        DeterministicAction {
            compiler: ToolchainIdentity {
                sha256: "compiler".to_owned(),
                size_bytes: 1,
                version: "rustc test".to_owned(),
            },
            platform: PlatformIdentity {
                os: "test-os",
                arch: "test-arch",
            },
            working_directory: "workspace".to_owned(),
            arguments: arguments.iter().map(|value| (*value).to_owned()).collect(),
            environment: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: vec!["target/demo.rmeta".to_owned()],
        }
    }

    #[test]
    fn action_keys_are_deterministic_and_content_sensitive() {
        let first = action_key(&action(&["--crate-name", "demo"])).expect("first key");
        let same = action_key(&action(&["--crate-name", "demo"])).expect("same key");
        let changed = action_key(&action(&["--crate-name", "other"])).expect("changed key");
        assert_eq!(first, same);
        assert_ne!(first, changed);
    }

    #[test]
    fn eligibility_reasons_are_stable_and_deduplicated() {
        let eligibility =
            RemoteEligibility::from_reasons(vec!["z".to_owned(), "a".to_owned(), "z".to_owned()]);
        assert!(!eligibility.eligible);
        assert_eq!(eligibility.reasons, ["a", "z"]);
    }
}
