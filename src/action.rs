use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize)]
pub struct DeterministicAction {
    pub compiler: ToolchainIdentity,
    pub platform: PlatformIdentity,
    pub working_directory: String,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub inputs: Vec<ActionInput>,
    pub outputs: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolchainIdentity {
    pub sha256: String,
    pub size_bytes: u64,
    pub version: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlatformIdentity {
    pub os: &'static str,
    pub arch: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActionInput {
    pub path: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
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

/// The version of the action cache layout that keys belong to. Raised with
/// the manifest version whenever the rules for what may be stored tighten,
/// so no entry stored under the older rules is even looked up.
const ACTION_KEY_NAMESPACE: &str = "cargo-reapi-action-v3\0";

pub fn action_key(action: &DeterministicAction) -> Result<String> {
    let encoded = serde_json::to_vec(action).context("serializing deterministic action")?;
    // Keys are namespaced by the cache layout's version, so an entry an older
    // cargo-reapi wrote under the same action is never looked up.
    let mut hasher = Sha256::new();
    hasher.update(ACTION_KEY_NAMESPACE.as_bytes());
    hasher.update(encoded);
    Ok(format!("{:x}", hasher.finalize()))
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
    fn action_keys_invalidate_every_declared_correctness_dimension() {
        let baseline = action(&["--crate-name", "demo"]);
        let baseline_key = action_key(&baseline).expect("baseline key");

        let mut mutations = Vec::new();
        let mut toolchain = baseline.clone();
        toolchain.compiler.sha256 = "different-toolchain".to_owned();
        mutations.push(toolchain);
        // PATH is keyed by the directories it resolves through, so the
        // compiler's own identity must separate toolchains: its version and
        // host, and an explicit compilation target.
        let mut version = baseline.clone();
        version.compiler.version =
            "rustc 1.94.0\nhost: x86_64-unknown-linux-gnu\nrelease: 1.94.0".to_owned();
        mutations.push(version);
        let mut target = baseline.clone();
        target.arguments.extend([
            "--target".to_owned(),
            "aarch64-unknown-linux-gnu".to_owned(),
        ]);
        mutations.push(target);
        let mut platform = baseline.clone();
        platform.platform.arch = "different-arch";
        mutations.push(platform);
        let mut environment = baseline.clone();
        environment
            .environment
            .insert("CARGO_FEATURE_TEST".to_owned(), "1".to_owned());
        mutations.push(environment);
        let mut input = baseline.clone();
        input.inputs.push(ActionInput {
            path: "package/src/lib.rs".to_owned(),
            sha256: "different-source".to_owned(),
            size_bytes: 9,
        });
        mutations.push(input);
        let mut output = baseline.clone();
        output.outputs.push("target/demo.d".to_owned());
        mutations.push(output);

        for mutation in mutations {
            assert_ne!(
                action_key(&mutation).expect("mutated key"),
                baseline_key,
                "declared correctness dimension did not invalidate the key"
            );
        }
    }

    #[test]
    fn an_action_keyed_under_the_previous_namespace_has_another_key() {
        let action = action(&["--crate-name", "demo"]);
        let mut previous = Sha256::new();
        previous.update(b"cargo-reapi-action-v2\0");
        previous.update(serde_json::to_vec(&action).expect("encoded action"));

        assert_ne!(
            action_key(&action).expect("key"),
            format!("{:x}", previous.finalize())
        );
    }

    #[test]
    fn eligibility_reasons_are_stable_and_deduplicated() {
        let eligibility =
            RemoteEligibility::from_reasons(vec!["z".to_owned(), "a".to_owned(), "z".to_owned()]);
        assert!(!eligibility.eligible);
        assert_eq!(eligibility.reasons, ["a", "z"]);
    }
}
