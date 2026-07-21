use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const EMBEDDED_CONTRACT: &str = include_str!("../acceptance/contract.toml");
pub const EMBEDDED_CRITERIA: &str = include_str!("../acceptance/ACCEPTANCE_CRITERIA.md");

pub fn criteria_digest() -> String {
    criteria_digest_for(EMBEDDED_CRITERIA).expect("embedded criteria have one top status line")
}

fn criteria_digest_for(document: &str) -> Result<String> {
    let mut status_lines = 0;
    let mut normalized = String::with_capacity(document.len());
    for (index, line) in document.split_inclusive('\n').enumerate() {
        if line.starts_with("Status:") {
            status_lines += 1;
            if index > 2 {
                bail!("acceptance status must appear at the top of the criteria document");
            }
            normalized.push_str("Status: <non-normative qualification state>\n");
        } else {
            normalized.push_str(line);
        }
    }
    if status_lines != 1 {
        bail!("acceptance criteria must contain exactly one status statement");
    }
    Ok(format!("{:x}", Sha256::digest(normalized.as_bytes())))
}

pub fn criteria_file_identity(path: &Path) -> Result<(String, String)> {
    let bytes = fs::read(path)
        .with_context(|| format!("reading acceptance criteria {}", path.display()))?;
    let document = std::str::from_utf8(&bytes)
        .with_context(|| format!("acceptance criteria are not UTF-8: {}", path.display()))?;
    Ok((
        criteria_digest_for(document)?,
        format!("{:x}", Sha256::digest(&bytes)),
    ))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct AcceptanceContract {
    pub schema_version: u32,
    pub name: String,
    pub platform_os: String,
    pub platform_arch: String,
    pub minimum_logical_cpus: usize,
    pub minimum_memory_gib: u64,
    pub minimum_bro_concurrency: usize,
    pub admission_stress_multiplier: usize,
    pub scheduler_property_population: usize,
    pub storage_profiles: BTreeMap<String, StorageProfileContract>,
    pub maximum_build_rss_gib: u64,
    pub maximum_swap_growth_mib: u64,
    pub stall_seconds: u64,
    pub cache_time_to_live_seconds: u64,
    pub allow_threshold_overrides: bool,
    pub allow_gate_admission_limit: bool,
    pub require_clean_repositories: bool,
    pub require_empty_consumer_targets: bool,
    pub require_producer_exit_before_persistent_restore: bool,
    pub require_zero_warm_physical_actions: bool,
    pub require_external_rustc_observation: bool,
    pub require_mixed_key_physical_overlap: bool,
    pub require_adversarial_invalidation: bool,
    pub require_bevy_fixture: bool,
    pub require_real_moria: bool,
    pub canonical_moria_gate: Vec<String>,
    pub allowed_local_ineligible_reasons: Vec<String>,
    pub required_clause_prefixes: Vec<String>,
    pub clauses: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageProfileContract {
    pub single: u64,
    pub five: u64,
    pub stress: u64,
}

impl AcceptanceContract {
    pub fn embedded() -> Result<Self> {
        let contract: Self = toml::from_str(EMBEDDED_CONTRACT)
            .context("parsing embedded acceptance/contract.toml")?;
        contract.validate()?;
        Ok(contract)
    }

    pub fn digest() -> String {
        format!("{:x}", Sha256::digest(EMBEDDED_CONTRACT.as_bytes()))
    }

    pub fn verify_file(path: &Path) -> Result<()> {
        let bytes = fs::read(path)
            .with_context(|| format!("reading acceptance contract {}", path.display()))?;
        if bytes != EMBEDDED_CONTRACT.as_bytes() {
            bail!(
                "acceptance contract does not match the contract embedded in this binary: {}",
                path.display()
            );
        }
        Self::embedded().map(|_| ())
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!(
                "unsupported acceptance contract schema {}",
                self.schema_version
            );
        }
        if self.minimum_bro_concurrency < 5 {
            bail!("minimum Bro concurrency may not be lower than five");
        }
        if self.admission_stress_multiplier < 2 {
            bail!("admission stress must run at no less than 2N");
        }
        if self.allow_threshold_overrides || self.allow_gate_admission_limit {
            bail!("production acceptance may not expose threshold or gate-limit escape clauses");
        }
        if self.cache_time_to_live_seconds != 0 {
            bail!("the artifact cache may not use time-based expiry");
        }
        let ssd = self
            .storage_profiles
            .get("ssd")
            .context("acceptance contract requires the SSD storage profile")?;
        let rotational = self
            .storage_profiles
            .get("rotational")
            .context("acceptance contract requires the rotational storage profile")?;
        if self.storage_profiles.len() != 2
            || ssd.single != 60
            || ssd.five != 120
            || ssd.stress != 120
            || rotational.single != 300
            || rotational.five != 900
            || rotational.stress != 1_800
        {
            bail!("storage profiles are immutable: SSD=60/120/120s and rotational=300/900/1800s");
        }
        if self.canonical_moria_gate.len() != 4 {
            bail!("the canonical Moria gate must contain exactly four commands");
        }
        for required in &self.required_clause_prefixes {
            let specified = self
                .clauses
                .iter()
                .any(|value| value.starts_with(&format!("{required}-")));
            if !specified {
                bail!("acceptance contract is missing required clause family {required}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AcceptanceContract, EMBEDDED_CRITERIA, criteria_digest, criteria_digest_for};

    #[test]
    fn embedded_contract_has_no_durability_or_concurrency_escape_clauses() {
        let contract = AcceptanceContract::embedded().expect("valid embedded contract");
        assert_eq!(contract.cache_time_to_live_seconds, 0);
        assert!(!contract.allow_threshold_overrides);
        assert!(!contract.allow_gate_admission_limit);
        assert!(contract.require_producer_exit_before_persistent_restore);
        assert!(contract.require_zero_warm_physical_actions);
        assert!(contract.require_external_rustc_observation);
        assert!(contract.require_mixed_key_physical_overlap);
        assert!(contract.require_adversarial_invalidation);
        assert!(contract.minimum_bro_concurrency >= 5);
        assert!(contract.admission_stress_multiplier >= 2);
        assert_eq!(contract.storage_profiles.len(), 2);
        assert_eq!(contract.storage_profiles["ssd"].single, 60);
        assert_eq!(contract.storage_profiles["rotational"].stress, 1_800);
        assert!(contract.clauses.iter().any(|clause| clause.starts_with(
            "PAR-1: Five independent clean Moria worktrees start the entire canonical gate simultaneously"
        )));
        assert!(contract.clauses.iter().any(|clause| clause.starts_with(
            "SCALE-1: Ten independent clean Moria worktrees (2N) are admitted simultaneously"
        )));
    }

    #[test]
    fn criteria_are_embedded_in_the_binary() {
        assert_eq!(criteria_digest().len(), 64);
    }

    #[test]
    fn status_changes_do_not_change_normative_criteria_identity() {
        let changed = EMBEDDED_CRITERIA.replacen(
            EMBEDDED_CRITERIA
                .lines()
                .find(|line| line.starts_with("Status:"))
                .expect("status line"),
            "Status: **a different, non-normative qualification state.**",
            1,
        );
        assert_eq!(
            criteria_digest_for(EMBEDDED_CRITERIA).expect("original"),
            criteria_digest_for(&changed).expect("changed status")
        );
    }

    #[test]
    fn normative_changes_do_change_criteria_identity() {
        let changed = EMBEDDED_CRITERIA.replacen(
            "The aggregate verifier must reject",
            "The aggregate verifier should reject",
            1,
        );
        assert_ne!(
            criteria_digest_for(EMBEDDED_CRITERIA).expect("original"),
            criteria_digest_for(&changed).expect("changed criteria")
        );
    }

    #[test]
    fn criteria_reject_missing_duplicate_or_late_status_statements() {
        assert!(criteria_digest_for(&EMBEDDED_CRITERIA.replace("Status:", "State:")).is_err());
        assert!(criteria_digest_for(&format!("{EMBEDDED_CRITERIA}Status: duplicate\n")).is_err());
        let late = EMBEDDED_CRITERIA.replacen("Status:", "\n\nStatus:", 1);
        assert!(criteria_digest_for(&late).is_err());
    }
}
