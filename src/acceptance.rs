use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const EMBEDDED_CONTRACT: &str = include_str!("../acceptance/contract.toml");

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
    pub single_warm_deadline_seconds: u64,
    pub population_warm_deadline_seconds: u64,
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
    pub require_mixed_key_physical_overlap: bool,
    pub require_bevy_fixture: bool,
    pub require_real_moria: bool,
    pub canonical_moria_gate: Vec<String>,
    pub allowed_local_ineligible_reasons: Vec<String>,
    pub required_clause_prefixes: Vec<String>,
    pub clauses: Vec<String>,
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
    use super::AcceptanceContract;

    #[test]
    fn embedded_contract_has_no_durability_or_concurrency_escape_clauses() {
        let contract = AcceptanceContract::embedded().expect("valid embedded contract");
        assert_eq!(contract.cache_time_to_live_seconds, 0);
        assert!(!contract.allow_threshold_overrides);
        assert!(!contract.allow_gate_admission_limit);
        assert!(contract.require_producer_exit_before_persistent_restore);
        assert!(contract.require_zero_warm_physical_actions);
        assert!(contract.require_mixed_key_physical_overlap);
        assert!(contract.minimum_bro_concurrency >= 5);
        assert!(contract.admission_stress_multiplier >= 2);
        assert!(contract.clauses.iter().any(|clause| clause.starts_with(
            "PAR-1: Five independent clean Moria worktrees start the entire canonical gate simultaneously"
        )));
        assert!(contract.clauses.iter().any(|clause| clause.starts_with(
            "SCALE-1: Ten independent clean Moria worktrees (2N) are admitted simultaneously"
        )));
    }
}
