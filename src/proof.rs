use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::acceptance::{AcceptanceContract, criteria_digest};

pub const RECEIPT_SCHEMA_VERSION: u32 = 1;
pub const REQUIRED_RECEIPT_KINDS: &[&str] = &[
    "environment",
    "adversarial",
    "bevy-integrity",
    "coalescing",
    "resources",
    "portability",
    "moria-single",
    "moria-five",
    "moria-stress",
    "bro-five",
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReceiptIdentity {
    pub contract_sha256: String,
    pub criteria_sha256: String,
    pub implementation_tree_sha256: String,
    pub executable_sha256: String,
    pub harness_sha256: String,
    pub cargo_version: String,
    pub rustc_version: String,
    pub platform_os: String,
    pub platform_arch: String,
    pub run_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvidenceDigest {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AcceptanceReceipt {
    pub schema_version: u32,
    pub kind: String,
    pub identity: ReceiptIdentity,
    pub started_at_unix_ms: u128,
    pub completed_at_unix_ms: u128,
    pub raw_evidence: Vec<EvidenceDigest>,
    pub checks: BTreeMap<String, bool>,
    #[serde(default)]
    pub measurements: serde_json::Value,
    #[serde(default)]
    pub violations: Vec<String>,
    pub passed: bool,
}

#[derive(Debug, Serialize)]
pub struct CompleteProof {
    schema_version: u32,
    identity: Option<ReceiptIdentity>,
    required_receipts: Vec<String>,
    receipt_sha256: BTreeMap<String, String>,
    violations: Vec<String>,
    passed: bool,
}

impl CompleteProof {
    pub fn verify(receipts_dir: &Path) -> Result<Self> {
        let contract = AcceptanceContract::embedded()?;
        let mut identity: Option<ReceiptIdentity> = None;
        let mut receipt_sha256 = BTreeMap::new();
        let mut violations = Vec::new();

        for kind in REQUIRED_RECEIPT_KINDS {
            let path = receipts_dir.join(format!("{kind}.receipt.json"));
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    violations.push(format!(
                        "missing required receipt {}: {error}",
                        path.display()
                    ));
                    continue;
                }
            };
            receipt_sha256.insert((*kind).to_owned(), sha256_bytes(&bytes));
            let receipt: AcceptanceReceipt = match serde_json::from_slice(&bytes) {
                Ok(receipt) => receipt,
                Err(error) => {
                    violations.push(format!("invalid receipt {}: {error}", path.display()));
                    continue;
                }
            };
            verify_receipt(kind, &receipt, &contract, &mut violations);
            if let Some(expected) = &identity {
                if expected != &receipt.identity {
                    violations.push(format!(
                        "receipt {kind} has a mismatched acceptance identity"
                    ));
                }
            } else {
                identity = Some(receipt.identity.clone());
            }
            for evidence in &receipt.raw_evidence {
                match sha256_file(&evidence.path) {
                    Ok(actual) if actual == evidence.sha256 => {}
                    Ok(actual) => violations.push(format!(
                        "receipt {kind} evidence digest mismatch for {}: expected {}, got {actual}",
                        evidence.path.display(),
                        evidence.sha256
                    )),
                    Err(error) => violations.push(format!(
                        "receipt {kind} evidence unavailable at {}: {error:#}",
                        evidence.path.display()
                    )),
                }
            }
        }

        if let Some(identity) = &identity {
            if identity.contract_sha256 != AcceptanceContract::digest() {
                violations.push("receipt contract digest does not match this verifier".to_owned());
            }
            if identity.criteria_sha256 != criteria_digest() {
                violations.push("receipt criteria digest does not match this verifier".to_owned());
            }
            if identity.platform_os != contract.platform_os
                || identity.platform_arch != contract.platform_arch
            {
                violations
                    .push("receipt platform does not match the acceptance contract".to_owned());
            }
        } else {
            violations.push("no parseable acceptance receipt supplied an identity".to_owned());
        }

        Ok(Self {
            schema_version: RECEIPT_SCHEMA_VERSION,
            identity,
            required_receipts: REQUIRED_RECEIPT_KINDS
                .iter()
                .map(|kind| (*kind).to_owned())
                .collect(),
            receipt_sha256,
            passed: violations.is_empty(),
            violations,
        })
    }

    pub fn write_and_require_pass(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating proof directory {}", parent.display()))?;
        }
        fs::write(
            path,
            serde_json::to_vec_pretty(self).context("serializing complete proof")?,
        )
        .with_context(|| format!("writing complete proof {}", path.display()))?;
        if !self.passed {
            bail!(
                "aggregate acceptance failed closed; report: {}",
                path.display()
            );
        }
        Ok(())
    }
}

fn verify_receipt(
    expected_kind: &str,
    receipt: &AcceptanceReceipt,
    contract: &AcceptanceContract,
    violations: &mut Vec<String>,
) {
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION {
        violations.push(format!(
            "receipt {expected_kind} has unsupported schema {}",
            receipt.schema_version
        ));
    }
    if receipt.kind != expected_kind {
        violations.push(format!(
            "receipt {expected_kind} claims kind {}",
            receipt.kind
        ));
    }
    if receipt.completed_at_unix_ms < receipt.started_at_unix_ms {
        violations.push(format!(
            "receipt {expected_kind} completed before it started"
        ));
    }
    if receipt.raw_evidence.is_empty() {
        violations.push(format!(
            "receipt {expected_kind} has no independently retained raw evidence"
        ));
    }
    if receipt.checks.is_empty() || receipt.checks.values().any(|passed| !passed) {
        violations.push(format!(
            "receipt {expected_kind} has missing or failed checks"
        ));
    }
    for required in required_checks(expected_kind) {
        if receipt.checks.get(*required) != Some(&true) {
            violations.push(format!(
                "receipt {expected_kind} is missing required passing check {required}"
            ));
        }
    }
    verify_measurements(expected_kind, &receipt.measurements, violations);
    if !receipt.passed || !receipt.violations.is_empty() {
        violations.push(format!("receipt {expected_kind} did not pass cleanly"));
    }
    if receipt.identity.contract_sha256 != AcceptanceContract::digest()
        || receipt.identity.criteria_sha256 != criteria_digest()
        || receipt.identity.platform_os != contract.platform_os
        || receipt.identity.platform_arch != contract.platform_arch
    {
        violations.push(format!(
            "receipt {expected_kind} identity is stale or incompatible"
        ));
    }
    for (field, value) in [
        (
            "implementation_tree_sha256",
            &receipt.identity.implementation_tree_sha256,
        ),
        ("executable_sha256", &receipt.identity.executable_sha256),
        ("harness_sha256", &receipt.identity.harness_sha256),
        ("cargo_version", &receipt.identity.cargo_version),
        ("rustc_version", &receipt.identity.rustc_version),
        ("run_id", &receipt.identity.run_id),
    ] {
        if value.trim().is_empty() {
            violations.push(format!(
                "receipt {expected_kind} identity field {field} is empty"
            ));
        }
    }
}

fn required_checks(kind: &str) -> &'static [&'static str] {
    match kind {
        "environment" => &["host_contract", "toolchain_identity", "ssd_storage"],
        "adversarial" => &[
            "exact_mutation_set",
            "mutation_behavior",
            "poison_rejected",
            "rustflags_environment",
            "encoded_rustflags",
            "workspace_cargo_config",
            "ancestor_cargo_config",
            "cargo_home_config",
            "profile_change",
            "feature_change",
            "target_change",
            "external_path_dependency",
            "build_script_path_input",
            "build_script_environment",
            "proc_macro_environment",
            "undeclared_build_read_rejected",
            "undeclared_proc_macro_read_rejected",
            "network_rejected",
            "independent_process_observer",
        ],
        "bevy-integrity" => &[
            "application_parity",
            "test_enumeration_parity",
            "test_behavior_parity",
            "consumer_paths_only",
            "valid_signatures",
            "zero_os_compiler_linker",
        ],
        "coalescing" => &[
            "one_producer",
            "one_waiter",
            "waiter_behavior",
            "os_work_only_in_producer",
            "failing_producer_propagated",
            "no_partial_publish",
        ],
        "resources" => &[
            "shared_cross_process_ledger",
            "logical_gates_uncapped",
            "distinct_actions_overlap",
            "external_process_samples",
            "rss_within_limit",
            "swap_within_limit",
            "stall_is_infrastructure",
        ],
        "portability" => &[
            "macos_clone",
            "linux_reflink_or_fallback",
            "portable_copy_isolated",
        ],
        "moria-single" | "moria-five" | "moria-stress" => &[
            "clean_repositories",
            "producer_completed",
            "producer_deleted",
            "empty_consumer_targets",
            "canonical_gate_exact",
            "all_tests_passed",
            "simultaneous_start",
            "logical_gates_uncapped",
            "zero_physical_actions",
            "zero_os_compiler_linker",
            "deadline_met",
        ],
        "bro-five" => &[
            "public_cli_boundary",
            "bro_source_independent",
            "five_jobs_simultaneous",
            "canonical_gate_exact",
            "all_tests_passed",
            "zero_physical_actions",
            "zero_os_compiler_linker",
            "deadline_met",
        ],
        _ => &[],
    }
}

fn verify_measurements(kind: &str, measurements: &serde_json::Value, violations: &mut Vec<String>) {
    let number = |name: &str| measurements.get(name).and_then(serde_json::Value::as_u64);
    if kind == "resources" {
        for (name, limit) in [
            ("peak_aggregate_rss_bytes", 15 * 1024 * 1024 * 1024),
            ("swap_growth_bytes", 512 * 1024 * 1024),
        ] {
            if number(name).is_none_or(|value| value > limit) {
                violations.push(format!("resource receipt has invalid {name}"));
            }
        }
        if number("distinct_physical_overlap").is_none_or(|value| value < 2)
            || number("stall_seconds") != Some(300)
        {
            violations.push("resource receipt lacks required overlap or stall clock".to_owned());
        }
    }
    let population = match kind {
        "moria-single" => Some((1, 60_000)),
        "moria-five" | "bro-five" => Some((5, 120_000)),
        "moria-stress" => Some((10, 120_000)),
        _ => None,
    };
    if let Some((members, deadline_ms)) = population
        && (number("members").is_none_or(|value| value < members)
            || number("elapsed_ms").is_none_or(|value| value > deadline_ms)
            || number("os_compiler_linker_events") != Some(0)
            || number("physical_cacheable_actions") != Some(0))
    {
        violations.push(format!(
            "{kind} population measurements violate fixed acceptance"
        ));
    }
    if kind == "bevy-integrity"
        && (number("warm_elapsed_ms").is_none_or(|value| value > 60_000)
            || number("os_compiler_linker_events") != Some(0))
    {
        violations.push("Bevy integrity measurements violate fixed acceptance".to_owned());
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading evidence {}", path.display()))?;
    Ok(sha256_bytes(&bytes))
}

#[derive(Debug, Deserialize)]
struct LoggedEligibility {
    reasons: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LoggedAction {
    action_key: String,
    execution: String,
    exit_code: i32,
    cache_eligibility: LoggedEligibility,
}

#[derive(Debug, Serialize)]
pub struct ActionLogProof {
    schema_version: u32,
    contract_sha256: String,
    action_log: String,
    action_count: usize,
    execution_counts: BTreeMap<String, usize>,
    unique_action_keys: usize,
    cacheable_physical_actions: usize,
    externally_observed_compiler_actions: usize,
    violations: Vec<String>,
    passed: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum PopulationKind {
    Single,
    Five,
    Stress,
}

#[derive(Clone, Copy, Debug)]
pub enum StorageProfile {
    Ssd,
    Rotational,
}

impl StorageProfile {
    fn name(self) -> &'static str {
        match self {
            Self::Ssd => "ssd",
            Self::Rotational => "rotational",
        }
    }
}

#[derive(Debug, Deserialize)]
struct PopulationEvidence {
    schema_version: u32,
    members: Vec<PopulationMember>,
}

#[derive(Debug, Deserialize)]
struct PopulationMember {
    id: String,
    started_at_unix_ms: u128,
    completed_at_unix_ms: u128,
    exit_code: i32,
    action_log: String,
    worktree: String,
    rustc_trace: String,
    target_empty_at_start: bool,
}

#[derive(Debug, Serialize)]
pub struct PopulationProof {
    schema_version: u32,
    contract_sha256: String,
    kind: String,
    storage_profile: String,
    expected_minimum_members: usize,
    observed_members: usize,
    all_started_before_any_completed: bool,
    all_targets_empty_at_start: bool,
    elapsed_ms: u128,
    deadline_ms: u128,
    member_action_proofs: Vec<ActionLogProof>,
    violations: Vec<String>,
    passed: bool,
}

#[derive(Debug, Serialize)]
pub struct EnvironmentProof {
    schema_version: u32,
    contract_sha256: String,
    platform_profile_sha256: String,
    storage_profile: String,
    single_warm_deadline_seconds: u64,
    population_warm_deadline_seconds: u64,
    stress_warm_deadline_seconds: u64,
    platform_os: String,
    platform_arch: String,
    logical_cpus: usize,
    physical_memory_bytes: u64,
    rustc_verbose_version: String,
    sandbox_provider: String,
    sandbox_provider_version: String,
    sandbox_provider_identity_sha256: String,
    violations: Vec<String>,
    passed: bool,
}

impl ActionLogProof {
    pub fn verify_with_trace(
        path: &Path,
        rustc_trace: Option<&Path>,
        worktree: Option<&Path>,
    ) -> Result<Self> {
        let contract = AcceptanceContract::embedded()?;
        let encoded = fs::read_to_string(path)
            .with_context(|| format!("reading action evidence {}", path.display()))?;
        let mut actions = Vec::new();
        for (index, line) in encoded.lines().enumerate() {
            actions.push(serde_json::from_str::<LoggedAction>(line).with_context(|| {
                format!(
                    "parsing action evidence {} line {}",
                    path.display(),
                    index + 1
                )
            })?);
        }
        if actions.is_empty() {
            bail!("action evidence is empty: {}", path.display());
        }

        let mut execution_counts = BTreeMap::new();
        let mut keys = std::collections::BTreeSet::new();
        let mut violations = Vec::new();
        let mut cacheable_physical_actions = 0;
        for action in &actions {
            *execution_counts
                .entry(action.execution.clone())
                .or_insert(0) += 1;
            keys.insert(&action.action_key);
            if action.exit_code != 0 {
                violations.push(format!(
                    "action {} exited {} ({})",
                    action.action_key, action.exit_code, action.execution
                ));
            }
            match action.execution.as_str() {
                "cache-hit" | "coalesced-hit" | "gate-snapshot-hit" | "coalesced-gate-hit" => {}
                "local-ineligible"
                    if !action.cache_eligibility.reasons.is_empty()
                        && action.cache_eligibility.reasons.iter().all(|reason| {
                            contract
                                .allowed_local_ineligible_reasons
                                .iter()
                                .any(|allowed| allowed == reason)
                        }) => {}
                "local-cache-miss" | "local-output-incomplete" => {
                    cacheable_physical_actions += 1;
                    violations.push(format!(
                        "cacheable physical action {} executed as {}",
                        action.action_key, action.execution
                    ));
                }
                execution => violations.push(format!(
                    "action {} has unacceptable execution classification {execution}",
                    action.action_key
                )),
            }
        }

        let externally_observed_compiler_actions = match (rustc_trace, worktree) {
            (Some(trace), Some(worktree)) => observed_compiler_actions(trace, worktree)?,
            (None, None) => 0,
            _ => bail!("rustc trace and worktree must be supplied together"),
        };
        if rustc_trace.is_some() && externally_observed_compiler_actions != 0 {
            violations.push(format!(
                "external rustc observer recorded {externally_observed_compiler_actions} compiler actions"
            ));
        }

        Ok(Self {
            schema_version: 1,
            contract_sha256: AcceptanceContract::digest(),
            action_log: path.display().to_string(),
            action_count: actions.len(),
            execution_counts,
            unique_action_keys: keys.len(),
            cacheable_physical_actions,
            externally_observed_compiler_actions,
            passed: violations.is_empty(),
            violations,
        })
    }

    pub fn write_and_require_pass(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating proof directory {}", parent.display()))?;
        }
        fs::write(
            path,
            serde_json::to_vec_pretty(self).context("serializing action-log proof")?,
        )
        .with_context(|| format!("writing proof report {}", path.display()))?;
        if !self.passed {
            bail!(
                "action evidence failed the embedded acceptance contract; report: {}",
                path.display()
            );
        }
        Ok(())
    }
}

impl PopulationProof {
    pub fn verify(
        path: &Path,
        kind: PopulationKind,
        storage_profile: StorageProfile,
    ) -> Result<Self> {
        let contract = AcceptanceContract::embedded()?;
        let encoded = fs::read(path)
            .with_context(|| format!("reading population evidence {}", path.display()))?;
        let evidence: PopulationEvidence = serde_json::from_slice(&encoded)
            .with_context(|| format!("parsing population evidence {}", path.display()))?;
        if evidence.schema_version != 1 {
            bail!(
                "unsupported population evidence schema {}",
                evidence.schema_version
            );
        }
        let expected = expected_population(&contract, kind);
        let mut violations = Vec::new();
        if evidence.members.len() < expected {
            violations.push(format!(
                "population contains {} members; {expected} are required",
                evidence.members.len()
            ));
        }
        let earliest_start = evidence
            .members
            .iter()
            .map(|member| member.started_at_unix_ms)
            .min()
            .unwrap_or(0);
        let latest_start = evidence
            .members
            .iter()
            .map(|member| member.started_at_unix_ms)
            .max()
            .unwrap_or(u128::MAX);
        let earliest_completion = evidence
            .members
            .iter()
            .map(|member| member.completed_at_unix_ms)
            .min()
            .unwrap_or(0);
        let latest_completion = evidence
            .members
            .iter()
            .map(|member| member.completed_at_unix_ms)
            .max()
            .unwrap_or(0);
        let simultaneous = !evidence.members.is_empty() && latest_start < earliest_completion;
        if !simultaneous {
            violations.push(
                "not all population members started before the first member completed".to_owned(),
            );
        }
        let elapsed_ms = latest_completion.saturating_sub(earliest_start);
        let all_targets_empty_at_start = evidence
            .members
            .iter()
            .all(|member| member.target_empty_at_start);
        if !all_targets_empty_at_start {
            violations.push("at least one consumer target was non-empty at gate start".to_owned());
        }
        let deadline_ms = population_deadline_ms(&contract, kind, storage_profile)?;
        if elapsed_ms > deadline_ms {
            violations.push(format!(
                "population elapsed {elapsed_ms} ms; deadline is {deadline_ms} ms"
            ));
        }

        let mut member_action_proofs = Vec::new();
        for member in &evidence.members {
            if member.completed_at_unix_ms < member.started_at_unix_ms {
                violations.push(format!("member {} completed before it started", member.id));
            }
            if member.exit_code != 0 {
                violations.push(format!("member {} exited {}", member.id, member.exit_code));
            }
            let proof = verify_population_member(member)?;
            if !proof.passed {
                violations.push(format!("member {} action evidence failed", member.id));
            }
            member_action_proofs.push(proof);
        }

        Ok(Self {
            schema_version: 1,
            contract_sha256: AcceptanceContract::digest(),
            kind: match kind {
                PopulationKind::Single => "single",
                PopulationKind::Five => "five",
                PopulationKind::Stress => "stress-2n",
            }
            .to_owned(),
            storage_profile: storage_profile.name().to_owned(),
            expected_minimum_members: expected,
            observed_members: evidence.members.len(),
            all_started_before_any_completed: simultaneous,
            all_targets_empty_at_start,
            elapsed_ms,
            deadline_ms,
            member_action_proofs,
            passed: violations.is_empty(),
            violations,
        })
    }

    pub fn write_and_require_pass(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating proof directory {}", parent.display()))?;
        }
        fs::write(
            path,
            serde_json::to_vec_pretty(self).context("serializing population proof")?,
        )
        .with_context(|| format!("writing population proof {}", path.display()))?;
        if !self.passed {
            bail!(
                "population evidence failed the embedded acceptance contract; report: {}",
                path.display()
            );
        }
        Ok(())
    }
}

fn expected_population(contract: &AcceptanceContract, kind: PopulationKind) -> usize {
    match kind {
        PopulationKind::Single => 1,
        PopulationKind::Five => contract.minimum_bro_concurrency,
        PopulationKind::Stress => {
            contract.minimum_bro_concurrency * contract.admission_stress_multiplier
        }
    }
}

fn population_deadline_ms(
    contract: &AcceptanceContract,
    kind: PopulationKind,
    storage_profile: StorageProfile,
) -> Result<u128> {
    let profile = contract
        .storage_profiles
        .get(storage_profile.name())
        .context("selected storage profile is absent from the acceptance contract")?;
    let seconds = match kind {
        PopulationKind::Single => profile.single,
        PopulationKind::Five => profile.five,
        PopulationKind::Stress => profile.stress,
    };
    Ok(u128::from(seconds) * 1_000)
}

fn verify_population_member(member: &PopulationMember) -> Result<ActionLogProof> {
    ActionLogProof::verify_with_trace(
        Path::new(&member.action_log),
        Some(Path::new(&member.rustc_trace)),
        Some(Path::new(&member.worktree)),
    )
}

impl EnvironmentProof {
    pub fn capture(storage_profile: StorageProfile, platform_profile: &Path) -> Result<Self> {
        let contract = AcceptanceContract::embedded()?;
        let profile_bytes = fs::read(platform_profile)
            .with_context(|| format!("reading platform profile {}", platform_profile.display()))?;
        let platform: crate::evidence::PlatformProfile =
            toml::from_str(&String::from_utf8(profile_bytes.clone())?)?;
        if platform.schema_version != 1
            || platform.base_contract_sha256 != AcceptanceContract::digest()
        {
            bail!("platform profile does not inherit the embedded acceptance contract");
        }
        let profile = contract
            .storage_profiles
            .get(storage_profile.name())
            .context("selected storage profile is absent from the acceptance contract")?;
        let logical_cpus = std::thread::available_parallelism()
            .map(usize::from)
            .context("reading host logical CPU count")?;
        let physical_memory_bytes = physical_memory_bytes()?;
        let rustc_verbose_version = rustc_verbose_version()?;
        let sandbox_provider_identity_sha256 = crate::hermetic::provider_identity_digest()?;
        let mut violations = Vec::new();
        if std::env::consts::OS != platform.platform_os {
            violations.push(format!(
                "host OS {} does not match {}",
                std::env::consts::OS,
                platform.platform_os
            ));
        }
        if std::env::consts::ARCH != platform.platform_arch {
            violations.push(format!(
                "host architecture {} does not match {}",
                std::env::consts::ARCH,
                platform.platform_arch
            ));
        }
        if logical_cpus < contract.minimum_logical_cpus {
            violations.push(format!(
                "host has {logical_cpus} logical CPUs; {} required",
                contract.minimum_logical_cpus
            ));
        }
        let required_memory = contract.minimum_memory_gib * 1024 * 1024 * 1024;
        if physical_memory_bytes < required_memory {
            violations.push(format!(
                "host has {physical_memory_bytes} memory bytes; {required_memory} required"
            ));
        }
        Ok(Self {
            schema_version: 1,
            contract_sha256: AcceptanceContract::digest(),
            platform_profile_sha256: format!("{:x}", Sha256::digest(&profile_bytes)),
            storage_profile: storage_profile.name().to_owned(),
            single_warm_deadline_seconds: profile.single,
            population_warm_deadline_seconds: profile.five,
            stress_warm_deadline_seconds: profile.stress,
            platform_os: std::env::consts::OS.to_owned(),
            platform_arch: std::env::consts::ARCH.to_owned(),
            logical_cpus,
            physical_memory_bytes,
            rustc_verbose_version,
            sandbox_provider: crate::hermetic::SRT_REQUIRED_PACKAGE.to_owned(),
            sandbox_provider_version: crate::hermetic::SRT_REQUIRED_VERSION.to_owned(),
            sandbox_provider_identity_sha256,
            passed: violations.is_empty(),
            violations,
        })
    }

    pub fn write_and_require_pass(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating proof directory {}", parent.display()))?;
        }
        fs::write(
            path,
            serde_json::to_vec_pretty(self).context("serializing environment proof")?,
        )
        .with_context(|| format!("writing environment proof {}", path.display()))?;
        if !self.passed {
            bail!(
                "environment failed the embedded acceptance contract; report: {}",
                path.display()
            );
        }
        Ok(())
    }
}

fn observed_compiler_actions(trace: &Path, worktree: &Path) -> Result<usize> {
    let canonical_worktree = fs::canonicalize(worktree)
        .with_context(|| format!("canonicalizing audited worktree {}", worktree.display()))?;
    let mut count = 0;
    for entry in fs::read_dir(trace)
        .with_context(|| format!("reading external rustc trace {}", trace.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let record = fs::read_to_string(entry.path())?;
        if !record.lines().any(|line| line == "kind=compile") {
            continue;
        }
        let cwd = record
            .lines()
            .find_map(|line| line.strip_prefix("cwd="))
            .context("external rustc trace record has no cwd")?;
        if !Path::new(cwd).starts_with(worktree) && !Path::new(cwd).starts_with(&canonical_worktree)
        {
            continue;
        }
        let cwd = fs::canonicalize(cwd)
            .with_context(|| format!("canonicalizing observed compiler cwd {cwd}"))?;
        if cwd.starts_with(&canonical_worktree) {
            count += 1;
        }
    }
    Ok(count)
}

fn physical_memory_bytes() -> Result<u64> {
    if cfg!(target_os = "macos") {
        let output = Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .context("reading macOS physical memory")?;
        if !output.status.success() {
            bail!("sysctl hw.memsize failed");
        }
        return String::from_utf8(output.stdout)
            .context("physical memory is not UTF-8")?
            .trim()
            .parse()
            .context("parsing physical memory bytes");
    }
    if cfg!(target_os = "linux") {
        let meminfo = fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
        let kib: u64 = meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))
            .and_then(|value| value.split_whitespace().next())
            .context("MemTotal is absent from /proc/meminfo")?
            .parse()
            .context("parsing Linux physical memory")?;
        return Ok(kib * 1024);
    }
    bail!("physical memory discovery is unsupported on this platform")
}

fn rustc_verbose_version() -> Result<String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("reading rustc toolchain identity")?;
    if !output.status.success() {
        bail!("rustc -vV failed");
    }
    String::from_utf8(output.stdout).context("rustc toolchain identity is not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::{
        AcceptanceContract, AcceptanceReceipt, ActionLogProof, CompleteProof, EvidenceDigest,
        PopulationKind, PopulationProof, REQUIRED_RECEIPT_KINDS, ReceiptIdentity, StorageProfile,
        population_deadline_ms, required_checks, sha256_file,
    };
    use crate::acceptance::criteria_digest;

    #[test]
    fn warm_proof_rejects_even_one_successful_local_cache_miss() {
        let fixture = tempfile::NamedTempFile::new().expect("action log");
        std::fs::write(
            fixture.path(),
            r#"{"action_key":"abc","execution":"local-cache-miss","exit_code":0,"cache_eligibility":{"reasons":[]}}
"#,
        )
        .expect("write action log");
        let proof =
            ActionLogProof::verify_with_trace(fixture.path(), None, None).expect("proof report");
        assert!(!proof.passed);
        assert_eq!(proof.cacheable_physical_actions, 1);
    }

    fn write_complete_receipt_set(root: &std::path::Path) {
        let evidence = root.join("raw.log");
        std::fs::write(&evidence, "independent evidence\n").expect("write evidence");
        let identity = ReceiptIdentity {
            contract_sha256: AcceptanceContract::digest(),
            criteria_sha256: criteria_digest(),
            implementation_tree_sha256: "implementation".to_owned(),
            executable_sha256: "executable".to_owned(),
            harness_sha256: "harness".to_owned(),
            cargo_version: "cargo test".to_owned(),
            rustc_version: "rustc test".to_owned(),
            platform_os: "macos".to_owned(),
            platform_arch: "aarch64".to_owned(),
            run_id: "unit-test-run".to_owned(),
        };
        for kind in REQUIRED_RECEIPT_KINDS {
            let receipt = AcceptanceReceipt {
                schema_version: 1,
                kind: (*kind).to_owned(),
                identity: identity.clone(),
                started_at_unix_ms: 1,
                completed_at_unix_ms: 2,
                raw_evidence: vec![EvidenceDigest {
                    path: evidence.clone(),
                    sha256: sha256_file(&evidence).expect("evidence digest"),
                }],
                checks: required_checks(kind)
                    .iter()
                    .map(|check| ((*check).to_owned(), true))
                    .collect(),
                measurements: match *kind {
                    "resources" => serde_json::json!({
                        "peak_aggregate_rss_bytes": 1,
                        "swap_growth_bytes": 0,
                        "distinct_physical_overlap": 2,
                        "stall_seconds": 300
                    }),
                    "moria-single" => {
                        serde_json::json!({"members":1,"elapsed_ms":1,"os_compiler_linker_events":0,"physical_cacheable_actions":0})
                    }
                    "moria-five" | "bro-five" => {
                        serde_json::json!({"members":5,"elapsed_ms":1,"os_compiler_linker_events":0,"physical_cacheable_actions":0})
                    }
                    "moria-stress" => {
                        serde_json::json!({"members":10,"elapsed_ms":1,"os_compiler_linker_events":0,"physical_cacheable_actions":0})
                    }
                    "bevy-integrity" => {
                        serde_json::json!({"warm_elapsed_ms":1,"os_compiler_linker_events":0})
                    }
                    _ => serde_json::json!({}),
                },
                violations: Vec::new(),
                passed: true,
            };
            std::fs::write(
                root.join(format!("{kind}.receipt.json")),
                serde_json::to_vec(&receipt).expect("serialize receipt"),
            )
            .expect("write receipt");
        }
    }

    #[test]
    fn aggregate_proof_requires_a_complete_consistent_receipt_set() {
        let directory = tempfile::tempdir().expect("receipt directory");
        write_complete_receipt_set(directory.path());
        let proof = CompleteProof::verify(directory.path()).expect("complete proof");
        assert!(proof.passed);
        assert!(proof.violations.is_empty());
    }

    #[test]
    fn aggregate_proof_rejects_missing_and_tampered_evidence() {
        let directory = tempfile::tempdir().expect("receipt directory");
        write_complete_receipt_set(directory.path());
        std::fs::remove_file(directory.path().join("bro-five.receipt.json"))
            .expect("remove receipt");
        std::fs::write(directory.path().join("raw.log"), "tampered\n").expect("tamper evidence");
        let proof = CompleteProof::verify(directory.path()).expect("complete proof");
        assert!(!proof.passed);
        assert!(
            proof
                .violations
                .iter()
                .any(|item| item.contains("missing required receipt"))
        );
        assert!(
            proof
                .violations
                .iter()
                .any(|item| item.contains("evidence digest mismatch"))
        );
    }

    #[test]
    fn external_compiler_observation_rejects_a_self_reported_hit() {
        let directory = tempfile::tempdir().expect("proof directory");
        let worktree = directory.path().join("worktree");
        let trace = directory.path().join("trace");
        std::fs::create_dir_all(&worktree).expect("worktree");
        std::fs::create_dir_all(&trace).expect("trace");
        let log = directory.path().join("actions.jsonl");
        std::fs::write(
            &log,
            r#"{"action_key":"abc","execution":"gate-snapshot-hit","exit_code":0,"cache_eligibility":{"reasons":[]}}
"#,
        )
        .expect("write action log");
        std::fs::write(
            trace.join("rustc-observation.unique-compile-record"),
            format!(
                "kind=compile\ncrate_name=fixture\ncwd={}\n",
                worktree.display()
            ),
        )
        .expect("write external trace");
        std::fs::write(
            trace.join("rustc-observation.unique-query-record"),
            format!(
                "kind=query\ncrate_name=control\ncwd={}\n",
                worktree.display()
            ),
        )
        .expect("write external query trace");
        let proof = ActionLogProof::verify_with_trace(&log, Some(&trace), Some(&worktree))
            .expect("proof report");
        assert!(!proof.passed);
        assert_eq!(proof.cacheable_physical_actions, 0);
        assert_eq!(proof.externally_observed_compiler_actions, 1);
    }

    #[test]
    fn population_proof_rejects_serial_waves_and_fewer_than_five_members() {
        let directory = tempfile::tempdir().expect("proof directory");
        let log = directory.path().join("actions.jsonl");
        let trace = directory.path().join("trace");
        let worktree = directory.path().join("worktree");
        std::fs::create_dir_all(&trace).expect("trace directory");
        std::fs::create_dir_all(&worktree).expect("worktree directory");
        std::fs::write(
            &log,
            r#"{"action_key":"abc","execution":"cache-hit","exit_code":0,"cache_eligibility":{"reasons":[]}}
"#,
        )
        .expect("write action log");
        let evidence = directory.path().join("population.json");
        std::fs::write(
            &evidence,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "members": [
                    {"id":"one","started_at_unix_ms":0,"completed_at_unix_ms":10,"exit_code":0,"action_log":log,"worktree":worktree,"rustc_trace":trace,"target_empty_at_start":true},
                    {"id":"two","started_at_unix_ms":11,"completed_at_unix_ms":20,"exit_code":0,"action_log":log,"worktree":worktree,"rustc_trace":trace,"target_empty_at_start":true}
                ]
            }))
            .expect("serialize evidence"),
        )
        .expect("write evidence");
        let proof = PopulationProof::verify(&evidence, PopulationKind::Five, StorageProfile::Ssd)
            .expect("proof");
        assert!(!proof.passed);
        assert!(!proof.all_started_before_any_completed);
        assert_eq!(proof.expected_minimum_members, 5);
    }

    #[test]
    fn storage_profiles_apply_fixed_deadlines_without_changing_correctness() {
        let directory = tempfile::tempdir().expect("proof directory");
        let log = directory.path().join("actions.jsonl");
        let trace = directory.path().join("trace");
        let worktree = directory.path().join("worktree");
        std::fs::create_dir_all(&trace).expect("trace directory");
        std::fs::create_dir_all(&worktree).expect("worktree directory");
        std::fs::write(
            &log,
            r#"{"action_key":"abc","execution":"gate-snapshot-hit","exit_code":0,"cache_eligibility":{"reasons":[]}}
"#,
        )
        .expect("write action log");
        let evidence = directory.path().join("population.json");
        std::fs::write(
            &evidence,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "members": [{
                    "id":"one",
                    "started_at_unix_ms":0,
                    "completed_at_unix_ms":269_194,
                    "exit_code":0,
                    "action_log":log,
                    "worktree":worktree,
                    "rustc_trace":trace
                    ,"target_empty_at_start":true
                }]
            }))
            .expect("serialize evidence"),
        )
        .expect("write evidence");

        let ssd = PopulationProof::verify(&evidence, PopulationKind::Single, StorageProfile::Ssd)
            .expect("SSD proof");
        let rotational = PopulationProof::verify(
            &evidence,
            PopulationKind::Single,
            StorageProfile::Rotational,
        )
        .expect("rotational proof");
        assert!(!ssd.passed);
        assert_eq!(ssd.deadline_ms, 60_000);
        assert!(rotational.passed);
        assert_eq!(rotational.deadline_ms, 300_000);
        let contract = AcceptanceContract::embedded().expect("contract");
        assert_eq!(
            population_deadline_ms(&contract, PopulationKind::Five, StorageProfile::Rotational)
                .expect("five deadline"),
            900_000
        );
        assert_eq!(
            population_deadline_ms(
                &contract,
                PopulationKind::Stress,
                StorageProfile::Rotational,
            )
            .expect("stress deadline"),
            1_800_000
        );
    }
}
