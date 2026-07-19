use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::acceptance::AcceptanceContract;

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
    violations: Vec<String>,
    passed: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum PopulationKind {
    Single,
    Five,
    Stress,
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
}

#[derive(Debug, Serialize)]
pub struct PopulationProof {
    schema_version: u32,
    contract_sha256: String,
    kind: String,
    expected_minimum_members: usize,
    observed_members: usize,
    all_started_before_any_completed: bool,
    elapsed_ms: u128,
    member_action_proofs: Vec<ActionLogProof>,
    violations: Vec<String>,
    passed: bool,
}

#[derive(Debug, Serialize)]
pub struct EnvironmentProof {
    schema_version: u32,
    contract_sha256: String,
    platform_os: String,
    platform_arch: String,
    logical_cpus: usize,
    physical_memory_bytes: u64,
    violations: Vec<String>,
    passed: bool,
}

impl ActionLogProof {
    pub fn verify(path: &Path) -> Result<Self> {
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
                "cache-hit" | "coalesced-hit" | "gate-snapshot-hit" => {}
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

        Ok(Self {
            schema_version: 1,
            contract_sha256: AcceptanceContract::digest(),
            action_log: path.display().to_string(),
            action_count: actions.len(),
            execution_counts,
            unique_action_keys: keys.len(),
            cacheable_physical_actions,
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
    pub fn verify(path: &Path, kind: PopulationKind) -> Result<Self> {
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
        let expected = match kind {
            PopulationKind::Single => 1,
            PopulationKind::Five => contract.minimum_bro_concurrency,
            PopulationKind::Stress => {
                contract.minimum_bro_concurrency * contract.admission_stress_multiplier
            }
        };
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
        let deadline_seconds = match kind {
            PopulationKind::Single => contract.single_warm_deadline_seconds,
            PopulationKind::Five | PopulationKind::Stress => {
                contract.population_warm_deadline_seconds
            }
        };
        let deadline_ms = u128::from(deadline_seconds) * 1_000;
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
            let proof = ActionLogProof::verify(Path::new(&member.action_log))?;
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
            expected_minimum_members: expected,
            observed_members: evidence.members.len(),
            all_started_before_any_completed: simultaneous,
            elapsed_ms,
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

impl EnvironmentProof {
    pub fn capture() -> Result<Self> {
        let contract = AcceptanceContract::embedded()?;
        let logical_cpus = std::thread::available_parallelism()
            .map(usize::from)
            .context("reading host logical CPU count")?;
        let physical_memory_bytes = physical_memory_bytes()?;
        let mut violations = Vec::new();
        if std::env::consts::OS != contract.platform_os {
            violations.push(format!(
                "host OS {} does not match {}",
                std::env::consts::OS,
                contract.platform_os
            ));
        }
        if std::env::consts::ARCH != contract.platform_arch {
            violations.push(format!(
                "host architecture {} does not match {}",
                std::env::consts::ARCH,
                contract.platform_arch
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
            platform_os: std::env::consts::OS.to_owned(),
            platform_arch: std::env::consts::ARCH.to_owned(),
            logical_cpus,
            physical_memory_bytes,
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

#[cfg(test)]
mod tests {
    use super::{ActionLogProof, PopulationKind, PopulationProof};

    #[test]
    fn warm_proof_rejects_even_one_successful_local_cache_miss() {
        let fixture = tempfile::NamedTempFile::new().expect("action log");
        std::fs::write(
            fixture.path(),
            r#"{"action_key":"abc","execution":"local-cache-miss","exit_code":0,"cache_eligibility":{"reasons":[]}}
"#,
        )
        .expect("write action log");
        let proof = ActionLogProof::verify(fixture.path()).expect("proof report");
        assert!(!proof.passed);
        assert_eq!(proof.cacheable_physical_actions, 1);
    }

    #[test]
    fn population_proof_rejects_serial_waves_and_fewer_than_five_members() {
        let directory = tempfile::tempdir().expect("proof directory");
        let log = directory.path().join("actions.jsonl");
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
                    {"id":"one","started_at_unix_ms":0,"completed_at_unix_ms":10,"exit_code":0,"action_log":log},
                    {"id":"two","started_at_unix_ms":11,"completed_at_unix_ms":20,"exit_code":0,"action_log":log}
                ]
            }))
            .expect("serialize evidence"),
        )
        .expect("write evidence");
        let proof = PopulationProof::verify(&evidence, PopulationKind::Five).expect("proof");
        assert!(!proof.passed);
        assert!(!proof.all_started_before_any_completed);
        assert_eq!(proof.expected_minimum_members, 5);
    }
}
