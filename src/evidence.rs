use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::acceptance::{AcceptanceContract, criteria_digest};

pub const EVIDENCE_SCHEMA_VERSION: u32 = 3;
pub const PLATFORM_IDS: &[&str] = &["macos-arm64", "linux-x86_64"];

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EvidenceStatus {
    Pass,
    Fail,
    Unmet,
    Unsubstantiated,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct EvidenceRef {
    pub role: String,
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RunProvenance {
    pub harness_identity: String,
    pub runner_path: String,
    pub runner_sha256: String,
    pub criteria_sha256: String,
    pub criteria_document_sha256: String,
    pub criteria_document_path: String,
    pub criteria_git_blob: String,
    pub criteria_commit: String,
    pub started_at_unix_ms: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlatformIdentity {
    pub contract_sha256: String,
    pub criteria_sha256: String,
    pub criteria_document_sha256: String,
    pub implementation_tree_sha256: String,
    pub source_revision: String,
    pub driver_sha256: String,
    pub auditor_sha256: String,
    pub cargo_version: String,
    pub rustc_version: String,
    pub platform_profile_sha256: String,
    pub platform_os: String,
    pub platform_arch: String,
    pub batch_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClaimEvidence {
    pub status: EvidenceStatus,
    pub evidence_roles: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AcceptanceReceiptV2 {
    pub schema_version: u32,
    pub kind: String,
    pub status: EvidenceStatus,
    pub identity: PlatformIdentity,
    pub provenance: RunProvenance,
    pub evidence_refs: Vec<EvidenceRef>,
    pub claims: BTreeMap<String, ClaimEvidence>,
    #[serde(default)]
    pub measurements: serde_json::Value,
    #[serde(default)]
    pub violations: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReceiptPointer {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformBatch {
    pub schema_version: u32,
    pub platform_id: String,
    pub status: EvidenceStatus,
    pub identity: PlatformIdentity,
    pub started_at_unix_ms: u128,
    pub completed_at_unix_ms: u128,
    pub receipts: BTreeMap<String, ReceiptPointer>,
    #[serde(default)]
    pub violations: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlatformProfile {
    pub schema_version: u32,
    pub id: String,
    pub base_contract_sha256: String,
    pub platform_os: String,
    pub platform_arch: String,
}

#[derive(Debug, Serialize)]
pub struct AggregateProofV2 {
    pub schema_version: u32,
    pub platforms: BTreeMap<String, PlatformResult>,
    pub violations: Vec<String>,
    pub passed: bool,
}

#[derive(Debug, Serialize)]
pub struct PlatformResult {
    pub status: EvidenceStatus,
    pub receipt_statuses: BTreeMap<String, EvidenceStatus>,
    pub verified_artifacts: usize,
    pub violations: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NestedEvidence {
    #[serde(default)]
    evidence_refs: Vec<EvidenceRef>,
}

impl AggregateProofV2 {
    pub fn verify(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing aggregate root {}", root.display()))?;
        let mut platforms = BTreeMap::new();
        let mut violations = Vec::new();
        let mut common: Option<(String, String, String, String)> = None;

        for platform_id in PLATFORM_IDS {
            let platform_root = root.join(platform_id);
            let result = verify_platform(&platform_root, platform_id, &mut common);
            match result {
                Ok(result) => {
                    for violation in &result.violations {
                        violations.push(format!("{platform_id}: {violation}"));
                    }
                    platforms.insert((*platform_id).to_owned(), result);
                }
                Err(error) => {
                    let message = format!("{platform_id}: platform batch is UNMET: {error:#}");
                    violations.push(message.clone());
                    platforms.insert(
                        (*platform_id).to_owned(),
                        PlatformResult {
                            status: EvidenceStatus::Unmet,
                            receipt_statuses: BTreeMap::new(),
                            verified_artifacts: 0,
                            violations: vec![message],
                        },
                    );
                }
            }
        }

        let passed = violations.is_empty()
            && platforms
                .values()
                .all(|result| result.status == EvidenceStatus::Pass);
        Ok(Self {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            platforms,
            violations,
            passed,
        })
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing aggregate proof {}", path.display()))
    }
}

fn verify_platform(
    root: &Path,
    expected_platform: &str,
    common: &mut Option<(String, String, String, String)>,
) -> Result<PlatformResult> {
    let batch_path = root.join("batch.json");
    let batch: PlatformBatch = serde_json::from_slice(
        &fs::read(&batch_path)
            .with_context(|| format!("reading platform batch {}", batch_path.display()))?,
    )?;
    let mut violations = Vec::new();
    if batch.schema_version != EVIDENCE_SCHEMA_VERSION {
        violations.push(format!("unsupported batch schema {}", batch.schema_version));
    }
    if batch.platform_id != expected_platform {
        violations.push(format!(
            "batch declares platform {} instead of {expected_platform}",
            batch.platform_id
        ));
    }
    if batch.status != EvidenceStatus::Pass {
        violations.push(format!("batch status is {:?}", batch.status));
    }
    if batch.completed_at_unix_ms < batch.started_at_unix_ms {
        violations.push("batch completed before it started".to_owned());
    }
    verify_platform_identity(&batch.identity, expected_platform, &mut violations)?;
    verify_platform_profile(root, &batch.identity, expected_platform, &mut violations)?;
    let common_identity = (
        batch.identity.contract_sha256.clone(),
        batch.identity.criteria_sha256.clone(),
        batch.identity.implementation_tree_sha256.clone(),
        batch.identity.source_revision.clone(),
    );
    if let Some(expected) = common {
        if expected != &common_identity {
            violations.push("platform batch has mismatched common identity".to_owned());
        }
    } else {
        *common = Some(common_identity);
    }

    let mut receipt_statuses = BTreeMap::new();
    let mut verified_artifacts = 0;
    for kind in required_receipts(expected_platform) {
        let Some(pointer) = batch.receipts.get(*kind) else {
            receipt_statuses.insert((*kind).to_owned(), EvidenceStatus::Unmet);
            violations.push(format!("required receipt {kind} is UNMET"));
            continue;
        };
        let receipt_path = secure_relative(root, &pointer.path)?;
        verify_digest(&receipt_path, &pointer.sha256)?;
        let receipt: AcceptanceReceiptV2 = serde_json::from_slice(&fs::read(&receipt_path)?)?;
        let mut receipt_violations = Vec::new();
        verify_receipt_v2(
            root,
            kind,
            &batch.identity,
            &receipt,
            &mut receipt_violations,
            &mut verified_artifacts,
        )?;
        let status = if receipt_violations.is_empty() {
            EvidenceStatus::Pass
        } else {
            for violation in receipt_violations {
                violations.push(format!("receipt {kind}: {violation}"));
            }
            match receipt.status {
                EvidenceStatus::Unmet => EvidenceStatus::Unmet,
                EvidenceStatus::Unsubstantiated => EvidenceStatus::Unsubstantiated,
                EvidenceStatus::Pass | EvidenceStatus::Fail => EvidenceStatus::Fail,
            }
        };
        receipt_statuses.insert((*kind).to_owned(), status);
    }
    for kind in batch.receipts.keys() {
        if !required_receipts(expected_platform).contains(&kind.as_str()) {
            violations.push(format!("unexpected receipt {kind}"));
        }
    }
    for violation in &batch.violations {
        violations.push(format!("batch reported: {violation}"));
    }

    Ok(PlatformResult {
        status: if violations.is_empty() {
            EvidenceStatus::Pass
        } else {
            EvidenceStatus::Fail
        },
        receipt_statuses,
        verified_artifacts,
        violations,
    })
}

fn verify_platform_identity(
    identity: &PlatformIdentity,
    expected_platform: &str,
    violations: &mut Vec<String>,
) -> Result<()> {
    if identity.contract_sha256 != AcceptanceContract::digest() {
        violations.push("contract digest does not match the embedded contract".to_owned());
    }
    if identity.criteria_sha256 != criteria_digest() {
        violations.push("criteria digest does not match the embedded criteria".to_owned());
    }
    if identity.criteria_document_sha256.trim().is_empty() {
        violations.push("exact criteria document digest is empty".to_owned());
    }
    let (os, arch) = match expected_platform {
        "macos-arm64" => ("macos", "aarch64"),
        "linux-x86_64" => ("linux", "x86_64"),
        _ => bail!("unsupported platform {expected_platform}"),
    };
    if identity.platform_os != os || identity.platform_arch != arch {
        violations.push(format!(
            "platform identity is {}/{} instead of {os}/{arch}",
            identity.platform_os, identity.platform_arch
        ));
    }
    for (name, value) in [
        (
            "implementation_tree_sha256",
            &identity.implementation_tree_sha256,
        ),
        ("source_revision", &identity.source_revision),
        ("driver_sha256", &identity.driver_sha256),
        ("auditor_sha256", &identity.auditor_sha256),
        ("cargo_version", &identity.cargo_version),
        ("rustc_version", &identity.rustc_version),
        ("platform_profile_sha256", &identity.platform_profile_sha256),
        ("batch_id", &identity.batch_id),
    ] {
        if value.trim().is_empty() {
            violations.push(format!("identity field {name} is empty"));
        }
    }
    Ok(())
}

fn verify_platform_profile(
    root: &Path,
    identity: &PlatformIdentity,
    expected_platform: &str,
    violations: &mut Vec<String>,
) -> Result<()> {
    let path = root.join("platform.toml");
    let digest = sha256_file(&path)?;
    if digest != identity.platform_profile_sha256 {
        violations.push("platform profile digest does not match the batch identity".to_owned());
    }
    let profile: PlatformProfile = toml::from_str(&fs::read_to_string(&path)?)?;
    if profile.schema_version != 1
        || profile.id != expected_platform
        || profile.base_contract_sha256 != AcceptanceContract::digest()
    {
        violations
            .push("platform profile is stale or attempts to replace the base contract".to_owned());
    }
    let expected = match expected_platform {
        "macos-arm64" => ("macos", "aarch64"),
        "linux-x86_64" => ("linux", "x86_64"),
        _ => bail!("unsupported platform {expected_platform}"),
    };
    if profile.platform_os != expected.0 || profile.platform_arch != expected.1 {
        violations.push("platform profile declares the wrong OS or architecture".to_owned());
    }
    Ok(())
}

fn verify_receipt_v2(
    root: &Path,
    expected_kind: &str,
    identity: &PlatformIdentity,
    receipt: &AcceptanceReceiptV2,
    violations: &mut Vec<String>,
    verified_artifacts: &mut usize,
) -> Result<()> {
    if receipt.schema_version != EVIDENCE_SCHEMA_VERSION {
        violations.push(format!(
            "unsupported receipt schema {}",
            receipt.schema_version
        ));
    }
    if receipt.kind != expected_kind {
        violations.push(format!("receipt declares kind {}", receipt.kind));
    }
    if receipt.status != EvidenceStatus::Pass || !receipt.violations.is_empty() {
        violations.push(format!("receipt status is {:?}", receipt.status));
    }
    if &receipt.identity != identity {
        violations.push("receipt identity differs from its platform batch".to_owned());
    }
    if receipt.provenance.harness_identity != "intrinsic" {
        violations.push(format!(
            "harness identity is {} rather than intrinsic",
            receipt.provenance.harness_identity
        ));
    }
    if receipt.provenance.criteria_sha256 != identity.criteria_sha256 {
        violations.push("runner started under a different criteria digest".to_owned());
    }
    if receipt.provenance.criteria_document_sha256 != identity.criteria_document_sha256 {
        violations.push("runner started under a different exact criteria document".to_owned());
    }
    for (name, value) in [
        ("runner_path", &receipt.provenance.runner_path),
        ("runner_sha256", &receipt.provenance.runner_sha256),
        ("criteria_git_blob", &receipt.provenance.criteria_git_blob),
        ("criteria_commit", &receipt.provenance.criteria_commit),
        (
            "criteria_document_path",
            &receipt.provenance.criteria_document_path,
        ),
    ] {
        if value.trim().is_empty() {
            violations.push(format!("provenance field {name} is empty"));
        }
    }

    let mut roles = BTreeMap::<String, Vec<PathBuf>>::new();
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for evidence in &receipt.evidence_refs {
        verify_evidence(
            root,
            evidence,
            &mut roles,
            &mut visiting,
            &mut visited,
            verified_artifacts,
        )?;
    }
    let criteria_paths = roles.get("criteria_document");
    if criteria_paths.is_none_or(|paths| paths.len() != 1) {
        violations.push("receipt must bind exactly one criteria document".to_owned());
    } else if let Some(path) = criteria_paths.and_then(|paths| paths.first())
        && sha256_file(path)? != identity.criteria_document_sha256
    {
        violations.push("bound criteria document digest differs from run identity".to_owned());
    }
    for claim in required_claims(expected_kind, &identity.platform_os) {
        let Some(evidence) = receipt.claims.get(*claim) else {
            violations.push(format!("claim {claim} is UNMET"));
            continue;
        };
        if evidence.status != EvidenceStatus::Pass {
            violations.push(format!("claim {claim} is {:?}", evidence.status));
        }
        if evidence.evidence_roles.is_empty() {
            violations.push(format!("claim {claim} has no evidence roles"));
        }
        for role in &evidence.evidence_roles {
            if !roles.contains_key(role) {
                violations.push(format!(
                    "claim {claim} cites unavailable evidence role {role}"
                ));
            }
        }
    }
    verify_receipt_semantics(expected_kind, &receipt.measurements, &roles, violations)?;
    Ok(())
}

fn verify_evidence(
    root: &Path,
    evidence: &EvidenceRef,
    roles: &mut BTreeMap<String, Vec<PathBuf>>,
    visiting: &mut BTreeSet<PathBuf>,
    visited: &mut BTreeSet<PathBuf>,
    verified_artifacts: &mut usize,
) -> Result<()> {
    if evidence.role.trim().is_empty() {
        bail!("evidence role is empty");
    }
    let path = secure_relative(root, &evidence.path)?;
    roles
        .entry(evidence.role.clone())
        .or_default()
        .push(path.clone());
    verify_digest(&path, &evidence.sha256)?;
    if visited.contains(&path) {
        return Ok(());
    }
    if !visiting.insert(path.clone()) {
        bail!("evidence reference cycle at {}", evidence.path.display());
    }
    *verified_artifacts += 1;
    if path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        let nested: NestedEvidence = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("parsing nested evidence {}", path.display()))?;
        for child in &nested.evidence_refs {
            verify_evidence(root, child, roles, visiting, visited, verified_artifacts)?;
        }
    }
    visiting.remove(&path);
    visited.insert(path);
    Ok(())
}

fn verify_receipt_semantics(
    kind: &str,
    measurements: &serde_json::Value,
    roles: &BTreeMap<String, Vec<PathBuf>>,
    violations: &mut Vec<String>,
) -> Result<()> {
    let read = |role: &str| -> Result<serde_json::Value> {
        let path = roles
            .get(role)
            .and_then(|paths| paths.first())
            .with_context(|| format!("semantic evidence role {role} is absent"))?;
        serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("parsing semantic evidence {}", path.display()))
    };
    let require_pass = |role: &str, violations: &mut Vec<String>| -> Result<serde_json::Value> {
        let report = read(role)?;
        if report.get("passed").and_then(serde_json::Value::as_bool) != Some(true)
            || report
                .get("violations")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|items| !items.is_empty())
        {
            violations.push(format!("{role} does not report a clean pass"));
        }
        Ok(report)
    };
    match kind {
        "environment" => {
            let report = require_pass("environment_report", violations)?;
            let platform = require_pass("platform_environment", violations)?;
            if report
                .get("storage_profile")
                .and_then(serde_json::Value::as_str)
                != Some("ssd")
            {
                violations.push("environment is not the required SSD qualification".to_owned());
            }
            for field in ["sandbox_provider_identity_sha256", "rustc_verbose_version"] {
                if report
                    .get(field)
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    violations.push(format!("environment field {field} is absent"));
                }
            }
            for field in [
                "sandbox_mechanism",
                "process_observer",
                "kernel",
                "cargo",
                "rustc",
            ] {
                if platform
                    .get(field)
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    violations.push(format!("platform environment field {field} is absent"));
                }
            }
            if report
                .get("platform_os")
                .and_then(serde_json::Value::as_str)
                == Some("linux")
                && (!roles.contains_key("cache_filesystem")
                    || !roles.contains_key("worktree_filesystem")
                    || !roles.contains_key("container_image_inspect")
                    || !roles.contains_key("qualification_container_inspect")
                    || !roles.contains_key("host_userns_policy_before")
                    || !roles.contains_key("host_userns_policy_during"))
            {
                violations
                    .push("Linux environment lacks cache/worktree filesystem evidence".to_owned());
            }
            if report
                .get("platform_os")
                .and_then(serde_json::Value::as_str)
                == Some("linux")
            {
                let during = roles
                    .get("host_userns_policy_during")
                    .and_then(|paths| paths.first())
                    .map(fs::read_to_string)
                    .transpose()?
                    .unwrap_or_default();
                if during.trim() != "0" {
                    violations.push(
                        "Linux nested user-namespace policy was not qualified in fail-closed mode"
                            .to_owned(),
                    );
                }
                let container = read("qualification_container_inspect")?;
                let host_config = container
                    .as_array()
                    .and_then(|items| items.first())
                    .and_then(|item| item.get("HostConfig"));
                let security_options = host_config
                    .and_then(|config| config.get("SecurityOpt"))
                    .and_then(serde_json::Value::as_array);
                if host_config
                    .and_then(|config| config.get("NetworkMode"))
                    .and_then(serde_json::Value::as_str)
                    != Some("none")
                    || host_config
                        .and_then(|config| config.get("Privileged"))
                        .and_then(serde_json::Value::as_bool)
                        != Some(false)
                    || host_config
                        .and_then(|config| config.get("CapDrop"))
                        .and_then(serde_json::Value::as_array)
                        .is_none_or(|caps| caps.iter().all(|cap| cap.as_str() != Some("ALL")))
                    || security_options.is_none_or(|options| {
                        options.iter().all(|option| {
                            option
                                .as_str()
                                .is_none_or(|value| !value.contains("no-new-privileges"))
                        })
                    })
                {
                    violations
                        .push("Linux outer qualification container is not fail-closed".to_owned());
                }
            }
        }
        "adversarial" => {
            let audit = require_pass("exact_mutation_os_audit", violations)?;
            let expected = BTreeSet::from([
                "adversarial_app".to_owned(),
                "leaf".to_owned(),
                "mid".to_owned(),
            ]);
            let set = json_string_set(&audit, "os_derived_crates");
            if set != expected || json_string_set(&audit, "wrapper_derived_crates") != expected {
                violations.push(format!(
                    "exact mutation attribution is {set:?}, expected {expected:?}"
                ));
            }
            if audit.get("expected").and_then(serde_json::Value::as_str) != Some("attribution")
                || audit
                    .get("attribution_sets_equal")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                || audit
                    .get("invalid_event_count")
                    .and_then(serde_json::Value::as_u64)
                    != Some(0)
            {
                violations
                    .push("OS and wrapper mutation attribution did not agree cleanly".to_owned());
            }
            require_log_tests(
                roles,
                "adversarial_suite_log",
                &[
                    "poisoned_dependency_makes_the_restored_gate_say_no",
                    "profile_environment_and_cargo_config_flags_all_invalidate",
                    "path_dependency_outside_worktree_invalidates_snapshot",
                    "declared_external_build_script_input_invalidates_snapshot",
                    "proc_macro_environment_change_invalidates_compiler_action",
                    "undeclared_external_build_script_read_fails_closed_without_publishing",
                    "undeclared_proc_macro_filesystem_read_fails_closed",
                    "deterministic_local_network_input_is_rejected_and_not_published",
                ],
                violations,
            )?;
        }
        "bevy-integrity" => {
            let report = require_pass("bevy_integrity_report", violations)?;
            for field in ["application_parity", "test_parity", "consumer_paths_only"] {
                if report.get(field).and_then(serde_json::Value::as_bool) != Some(true) {
                    violations.push(format!("Bevy integrity field {field} is not true"));
                }
            }
            if report
                .pointer("/restored/warm_elapsed_ms")
                .and_then(serde_json::Value::as_u64)
                .is_none()
                || report
                    .pointer("/restored/wrapper_compile_events")
                    .and_then(serde_json::Value::as_u64)
                    != Some(0)
                || report.get("restored").and_then(|v| v.get("application"))
                    != report.get("fresh").and_then(|v| v.get("application"))
                || report.get("restored").and_then(|v| v.get("test_list"))
                    != report.get("fresh").and_then(|v| v.get("test_list"))
                || report.get("restored").and_then(|v| v.get("test_behavior"))
                    != report.get("fresh").and_then(|v| v.get("test_behavior"))
            {
                violations
                    .push("restored Bevy outputs are not an exact fresh-control match".to_owned());
            }
            verify_zero_exec_audit(&require_pass("warm_os_audit", violations)?, violations);
        }
        "coalescing" => {
            let audit = require_pass("coalescing_os_audit", violations)?;
            let counts = audit
                .get("coalescing_root_event_counts")
                .and_then(serde_json::Value::as_object);
            if counts.is_none_or(|counts| {
                counts.len() != 2
                    || counts
                        .values()
                        .filter(|v| v.as_u64().is_some_and(|n| n > 0))
                        .count()
                        != 1
                    || counts.values().filter(|v| v.as_u64() == Some(0)).count() != 1
            }) {
                violations.push(
                    "OS coalescing distribution is not one producer and one waiter".to_owned(),
                );
            }
            let result = require_pass("coalescing_result", violations)?;
            let members = result.get("members").and_then(serde_json::Value::as_array);
            if members.is_none_or(|members| {
                members.len() != 2
                    || members
                        .iter()
                        .filter(|m| {
                            m.get("coalesced").and_then(serde_json::Value::as_bool) == Some(true)
                        })
                        .count()
                        != 1
                    || members.iter().any(|m| {
                        m.get("behavior_passed")
                            .and_then(serde_json::Value::as_bool)
                            != Some(true)
                    })
            }) {
                violations.push(
                    "coalescing result is not one successful waiter among two members".to_owned(),
                );
            }
            require_log_tests(
                roles,
                "adversarial_suite_log",
                &["failing_simultaneous_gates_all_fail_and_publish_nothing"],
                violations,
            )?;
        }
        "resources" => {
            let report = require_pass("resource_report", violations)?;
            if number(&report, "peak_aggregate_rss_bytes")
                .is_none_or(|n| n > 15 * 1024 * 1024 * 1024)
                || number(&report, "swap_growth_bytes").is_none_or(|n| n > 512 * 1024 * 1024)
                || number(&report, "peak_simultaneous_progress_processes").is_none_or(|n| n < 2)
                || number(&report, "observed_lease_owners").is_none_or(|n| n == 0)
                || report
                    .get("observed_action_identities")
                    .and_then(serde_json::Value::as_array)
                    .is_none_or(|items| items.len() < 2)
                || report
                    .get("infrastructure_stall")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
            {
                violations.push(
                    "resource report violates fixed RSS, swap, overlap, or no-stall bounds"
                        .to_owned(),
                );
            }
            let stall = read("stall_report")?;
            if number(&stall, "stall_seconds") != Some(300)
                || stall
                    .get("infrastructure_stall")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                || number(&stall, "exit_code") == Some(0)
            {
                violations
                    .push("live 300-second stall was not terminated as infrastructure".to_owned());
            }
        }
        "portable-copy-isolated" => require_log_tests(
            roles,
            "portable_copy_test_log",
            &["gate::tests::portable_snapshot_copy_is_a_complete_isolated_fallback"],
            violations,
        )?,
        "macos-clone" => {
            let traces = roles
                .get("clone_selection_trace")
                .context("clone trace absent")?;
            let mut selected = false;
            for line in fs::read_to_string(&traces[0])?.lines() {
                let event: serde_json::Value = serde_json::from_str(line)?;
                selected |= event
                    .get("selected_method")
                    .and_then(serde_json::Value::as_str)
                    == Some("copy-on-write")
                    && event
                        .get("attempt_succeeded")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                    && event
                        .get("source_location")
                        .and_then(serde_json::Value::as_str)
                        == Some("src/gate.rs:clone_tree_with_preference");
            }
            if !selected {
                violations.push("APFS clone selection branch was not observed".to_owned());
            }
        }
        "linux-copy-mechanism" => {
            let report = require_pass("linux_copy_report", violations)?;
            if report
                .get("filesystem_type")
                .and_then(serde_json::Value::as_str)
                .is_none()
                || report
                    .get("selected_method")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
                || report
                    .get("mechanism_proven")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
            {
                violations.push("Linux copy mechanism is not proven".to_owned());
            }
        }
        "moria-single" | "moria-five" | "moria-stress" => {
            let proof = require_pass("population_proof", violations)?;
            let expected_members = match kind {
                "moria-single" => 1,
                "moria-five" => 5,
                _ => 10,
            };
            if number(&proof, "observed_members") != Some(expected_members)
                || proof
                    .get("all_started_before_any_completed")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                || proof
                    .get("all_targets_empty_at_start")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                || proof
                    .get("member_action_proofs")
                    .and_then(serde_json::Value::as_array)
                    .is_none_or(|members| {
                        members
                            .iter()
                            .any(|m| number(m, "cacheable_physical_actions") != Some(0))
                    })
            {
                violations.push(format!("{kind} population semantics failed"));
            }
            verify_zero_exec_audit(
                &require_pass("population_os_audit", violations)?,
                violations,
            );
            if number(measurements, "members") != Some(expected_members)
                || number(measurements, "physical_cacheable_actions") != Some(0)
                || number(measurements, "elapsed_ms") != number(&proof, "elapsed_ms")
                || number(measurements, "deadline_ms") != number(&proof, "deadline_ms")
                || measurements
                    .get("performance_reference_met")
                    .and_then(serde_json::Value::as_bool)
                    != proof
                        .get("performance_reference_met")
                        .and_then(serde_json::Value::as_bool)
            {
                violations.push(format!(
                    "{kind} receipt measurements disagree with raw proof"
                ));
            }
        }
        "bro-five" => {
            let producer = require_pass("bro_producer", violations)?;
            let retirement = require_pass("producer_retirement", violations)?;
            let producer_audit = require_pass("producer_os_audit", violations)?;
            if producer
                .get("target_empty_at_start")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
                || number(&producer, "exit_code") != Some(0)
                || retirement
                    .get("producer_deleted")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                || retirement
                    .get("producer")
                    .and_then(serde_json::Value::as_str)
                    != producer.get("worktree").and_then(serde_json::Value::as_str)
            {
                violations.push(
                    "Bro exact-environment producer or retirement evidence is invalid".to_owned(),
                );
            }
            if producer_audit
                .get("expected")
                .and_then(serde_json::Value::as_str)
                != Some("nonzero")
                || number(&producer_audit, "selected_event_count").is_none_or(|count| count == 0)
                || number(&producer_audit, "invalid_event_count") != Some(0)
            {
                violations.push("Bro producer lacks clean nonzero OS build evidence".to_owned());
            }
            let proof = require_pass("bro_proof", violations)?;
            let population = require_pass("bro_population_proof", violations)?;
            if number(&proof, "observed_members").is_none_or(|n| n < 5)
                || proof
                    .get("all_started_before_any_completed")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                || number(&proof, "elapsed_ms").is_none()
                || number(&proof, "deadline_ms").is_none()
                || proof
                    .get("members")
                    .and_then(serde_json::Value::as_array)
                    .is_none_or(|members| {
                        members.len() < 5
                            || members.iter().any(|member| {
                                member.get("passed").and_then(serde_json::Value::as_bool)
                                    != Some(true)
                                    || member
                                        .get("target_empty_at_start")
                                        .and_then(serde_json::Value::as_bool)
                                        != Some(true)
                            })
                    })
                || population
                    .get("member_action_proofs")
                    .and_then(serde_json::Value::as_array)
                    .is_none_or(|members| {
                        members
                            .iter()
                            .any(|member| number(member, "cacheable_physical_actions") != Some(0))
                    })
            {
                violations
                    .push("Bro did not prove five simultaneous jobs within deadline".to_owned());
            }
            verify_zero_exec_audit(&require_pass("bro_os_audit", violations)?, violations);
        }
        _ => violations.push(format!(
            "no semantic verifier exists for receipt kind {kind}"
        )),
    }
    Ok(())
}

fn number(value: &serde_json::Value, field: &str) -> Option<u64> {
    value.get(field)?.as_u64()
}

fn json_string_set(value: &serde_json::Value, field: &str) -> BTreeSet<String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn verify_zero_exec_audit(audit: &serde_json::Value, violations: &mut Vec<String>) {
    if audit.get("expected").and_then(serde_json::Value::as_str) != Some("zero")
        || number(audit, "selected_event_count") != Some(0)
        || number(audit, "invalid_event_count") != Some(0)
    {
        violations.push("OS observer does not prove zero compiler/linker execs".to_owned());
    }
}

fn require_log_tests(
    roles: &BTreeMap<String, Vec<PathBuf>>,
    role: &str,
    tests: &[&str],
    violations: &mut Vec<String>,
) -> Result<()> {
    let paths = roles
        .get(role)
        .with_context(|| format!("log role {role} absent"))?;
    let log = fs::read_to_string(&paths[0])?;
    for test in tests {
        if !log.contains(&format!("test {test} ... ok")) {
            violations.push(format!("{role} does not contain passing test {test}"));
        }
    }
    Ok(())
}

fn secure_relative(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!(
            "evidence path is not a safe relative path: {}",
            relative.display()
        );
    }
    let root = root.canonicalize()?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("reading evidence metadata {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "evidence must be a regular non-symlink file: {}",
            path.display()
        );
    }
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(root) {
        bail!("evidence escapes the report root: {}", relative.display());
    }
    Ok(canonical)
}

fn verify_digest(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual != expected {
        bail!(
            "evidence digest mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            &fs::read(path).with_context(|| format!("reading evidence {}", path.display()))?
        )
    ))
}

pub fn required_receipts(platform: &str) -> &'static [&'static str] {
    match platform {
        "macos-arm64" => &[
            "environment",
            "adversarial",
            "bevy-integrity",
            "coalescing",
            "resources",
            "portable-copy-isolated",
            "macos-clone",
            "moria-single",
            "moria-five",
            "moria-stress",
            "bro-five",
        ],
        "linux-x86_64" => &[
            "environment",
            "adversarial",
            "bevy-integrity",
            "coalescing",
            "resources",
            "portable-copy-isolated",
            "linux-copy-mechanism",
            "moria-single",
            "moria-five",
            "moria-stress",
            "bro-five",
        ],
        _ => &[],
    }
}

fn required_claims(kind: &str, platform_os: &str) -> &'static [&'static str] {
    match kind {
        "environment" => &["host_contract", "toolchain_identity", "ssd_storage"],
        "adversarial" => &[
            "exact_mutation_set_os",
            "wrapper_attribution_crosscheck",
            "mutation_behavior",
            "poison_rejected",
            "flags_and_cargo_configuration",
            "external_and_generated_inputs",
            "undeclared_reads_rejected",
            "network_rejected",
        ],
        "bevy-integrity" if platform_os == "macos" => &[
            "application_parity",
            "test_behavior_parity",
            "consumer_paths_only",
            "valid_signatures",
            "zero_os_compiler_linker",
        ],
        "bevy-integrity" => &[
            "application_parity",
            "test_behavior_parity",
            "consumer_paths_only",
            "elf_integrity",
            "zero_os_compiler_linker",
        ],
        "coalescing" => &[
            "one_producer_one_waiter",
            "waiter_behavior",
            "os_work_only_in_producer",
            "failure_propagated",
            "no_partial_publish",
        ],
        "resources" => &[
            "shared_ledger",
            "distinct_actions_overlap",
            "rss_within_limit",
            "swap_within_limit",
            "stall_is_infrastructure",
        ],
        "portable-copy-isolated" => &["portable_copy_isolated"],
        "macos-clone" => &["copy_on_write_selected", "selection_source_identified"],
        "linux-copy-mechanism" => &[
            "filesystem_recorded",
            "mechanism_selected",
            "mechanism_proven",
        ],
        "moria-single" | "moria-five" | "moria-stress" => &[
            "producer_deleted",
            "empty_consumers",
            "canonical_gate",
            "simultaneous_start",
            "zero_physical_actions",
            "zero_os_compiler_linker",
            "performance_measured",
        ],
        "bro-five" => &[
            "public_cli_boundary",
            "exact_environment_producer",
            "producer_deleted",
            "five_jobs_simultaneous",
            "canonical_gate",
            "zero_physical_actions",
            "zero_os_compiler_linker",
            "performance_measured",
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::{EvidenceRef, secure_relative, sha256_file, verify_evidence};

    #[test]
    fn evidence_paths_reject_traversal_and_symlinks() {
        let root = tempdir().expect("root");
        assert!(secure_relative(root.path(), Path::new("../escape")).is_err());
        let target = root.path().join("target");
        fs::write(&target, "evidence").expect("target");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, root.path().join("link")).expect("link");
            assert!(secure_relative(root.path(), Path::new("link")).is_err());
        }
    }

    #[test]
    fn recursive_evidence_detects_nested_tampering() {
        let root = tempdir().expect("root");
        let raw = root.path().join("raw.jsonl");
        fs::write(&raw, "raw\n").expect("raw");
        let raw_digest = sha256_file(&raw).expect("raw digest");
        let report = root.path().join("report.json");
        fs::write(
            &report,
            serde_json::to_vec(&serde_json::json!({
                "evidence_refs": [{"role":"raw_os_events","path":"raw.jsonl","sha256":raw_digest}]
            }))
            .expect("report json"),
        )
        .expect("report");
        let reference = EvidenceRef {
            role: "auditor_report".to_owned(),
            path: PathBuf::from("report.json"),
            sha256: sha256_file(&report).expect("report digest"),
        };
        verify_evidence(
            root.path(),
            &reference,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut 0,
        )
        .expect("valid graph");
        fs::write(&raw, "tampered\n").expect("tamper");
        assert!(
            verify_evidence(
                root.path(),
                &reference,
                &mut Default::default(),
                &mut Default::default(),
                &mut Default::default(),
                &mut 0,
            )
            .is_err()
        );
    }
}
