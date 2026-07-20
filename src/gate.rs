use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::relocation::{RecordedPathMapping, execution_slot, replace_bytes};
#[cfg(target_os = "macos")]
use crate::resource::ResourceLease;

const SNAPSHOT_SCHEMA_VERSION: u32 = 15;

#[derive(Debug, Deserialize, Serialize)]
struct GateSnapshotManifest {
    schema_version: u32,
    key: String,
    producer_workspace: PathBuf,
    producer_target: PathBuf,
    path_mappings: Vec<RecordedPathMapping>,
    relocation_files: Vec<PathBuf>,
    observed_inputs: Vec<ObservedGateInput>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct ObservedGateInput {
    kind: String,
    name: String,
    sha256: String,
}

/// A durable snapshot of Cargo's complete target state for one exact logical gate.
///
/// The per-key lock only coalesces identical cold producers. Published snapshots
/// restore without taking this lock, so independent warm gates remain concurrent.
pub struct GateSnapshot {
    key: String,
    workspace: PathBuf,
    target: PathBuf,
    snapshot: PathBuf,
    resource_ledger: PathBuf,
    lock: Option<File>,
    restored: bool,
    coalesced: bool,
    clone_ms: u128,
    clone_method: &'static str,
    relocation_ms: u128,
    marker_hit: bool,
}

impl GateSnapshot {
    pub fn prepare(
        cache_root: &Path,
        workspace: &Path,
        target: &Path,
        action_log: &Path,
        cargo_args: &[OsString],
        declared_inputs: &[PathBuf],
    ) -> Result<Self> {
        let action_log = if action_log.is_absolute() {
            action_log.to_path_buf()
        } else {
            workspace.join(action_log)
        };
        let key = gate_key(
            cache_root,
            workspace,
            target,
            &action_log,
            cargo_args,
            declared_inputs,
        )?;
        let root = cache_root.join("gate-snapshots");
        fs::create_dir_all(root.join("locks"))?;
        fs::create_dir_all(root.join("objects"))?;
        let snapshot = root.join("objects").join(&key);
        let mut gate = Self {
            key: key.clone(),
            workspace: workspace.to_path_buf(),
            target: target.to_path_buf(),
            snapshot,
            resource_ledger: cache_root.join("resource-ledger-v1"),
            lock: None,
            restored: false,
            coalesced: false,
            clone_ms: 0,
            clone_method: "none",
            relocation_ms: 0,
            marker_hit: false,
        };
        if gate.is_published() && gate.observed_inputs_match()? {
            gate.restore_exact()?;
            return Ok(gate);
        }

        let lock_path = root.join("locks").join(format!("{key}.lock"));
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening gate snapshot lock {}", lock_path.display()))?;
        let waited = match lock.try_lock_exclusive() {
            Ok(()) => false,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                lock.lock_exclusive()
                    .with_context(|| format!("waiting for gate snapshot {key}"))?;
                true
            }
            Err(error) => return Err(error).context("trying gate snapshot lock"),
        };
        gate.lock = Some(lock);
        if gate.is_published() {
            if gate.observed_inputs_match()? {
                gate.coalesced = waited;
                gate.restore_exact()?;
                gate.unlock()?;
            } else {
                fs::remove_dir_all(&gate.snapshot)
                    .context("retiring a gate snapshot with stale declared inputs")?;
            }
        }
        Ok(gate)
    }

    pub fn publish_after_success(&mut self) -> Result<()> {
        if self.restored || self.lock.is_none() || self.is_published() {
            return self.unlock();
        }
        let parent = self
            .snapshot
            .parent()
            .context("gate snapshot has no object directory")?;
        let temporary = parent.join(format!(".{}.tmp-{}", self.key, std::process::id()));
        if temporary.exists() {
            fs::remove_dir_all(&temporary)?;
        }
        let observed_inputs = collect_observed_inputs(&self.target)?;
        stabilize_target_mtimes(&self.target)?;
        fs::create_dir_all(temporary.join("target"))?;
        clone_tree(&self.target, &temporary.join("target"))?;
        let generated = temporary.join("target/cargo-reapi");
        if generated.exists() {
            fs::remove_dir_all(&generated)?;
        }
        let path_mappings = read_path_mappings(&self.target)?;
        let relocation_files = build_relocation_index(
            &temporary.join("target"),
            &self.workspace,
            &self.target,
            &path_mappings,
        )?;
        let manifest = GateSnapshotManifest {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            key: self.key.clone(),
            producer_workspace: self.workspace.clone(),
            producer_target: self.target.clone(),
            path_mappings,
            relocation_files,
            observed_inputs,
        };
        fs::write(
            temporary.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        match fs::rename(&temporary, &self.snapshot) {
            Ok(()) => {}
            Err(error) if self.is_published() => {
                fs::remove_dir_all(&temporary).ok();
                let _ = error;
            }
            Err(error) => return Err(error).context("publishing gate snapshot"),
        }
        self.unlock()
    }

    pub fn record_successful_hit(&self, action_log: &Path) -> Result<()> {
        if !self.restored {
            return Ok(());
        }
        if let Some(parent) = action_log.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(action_log)
            .with_context(|| format!("opening action log {}", action_log.display()))?;
        log.lock_exclusive()?;
        writeln!(
            log,
            "{}",
            json!({
                "schema_version": 2,
                "action_key": self.key,
                "execution": if self.coalesced { "coalesced-gate-hit" } else { "gate-snapshot-hit" },
                "exit_code": 0,
                "cache_eligibility": {"eligible": true, "reasons": []},
                "snapshot_clone_ms": self.clone_ms,
                "snapshot_clone_method": self.clone_method,
                "snapshot_relocation_ms": self.relocation_ms,
                "snapshot_marker_hit": self.marker_hit,
            })
        )?;
        FileExt::unlock(&log)?;
        Ok(())
    }

    pub fn is_restored(&self) -> bool {
        self.restored
    }

    fn is_published(&self) -> bool {
        self.snapshot.join("manifest.json").is_file() && self.snapshot.join("target").is_dir()
    }

    fn observed_inputs_match(&self) -> Result<bool> {
        let manifest: GateSnapshotManifest = serde_json::from_slice(
            &fs::read(self.snapshot.join("manifest.json"))
                .with_context(|| format!("reading gate snapshot manifest for {}", self.key))?,
        )?;
        if manifest.schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Ok(false);
        }
        Ok(manifest
            .observed_inputs
            .iter()
            .all(|input| observed_input_digest(&input.kind, &input.name) == input.sha256))
    }

    fn restore_exact(&mut self) -> Result<()> {
        let selected = self.snapshot.clone();
        let selected_key = selected
            .file_name()
            .and_then(|name| name.to_str())
            .context("selected gate snapshot has no key")?;
        if self.target_marker_matches(selected_key) {
            self.restored = true;
            self.marker_hit = true;
            return Ok(());
        }
        let manifest: GateSnapshotManifest = serde_json::from_slice(
            &fs::read(selected.join("manifest.json"))
                .with_context(|| format!("reading gate snapshot manifest for {}", self.key))?,
        )?;
        if manifest.schema_version != SNAPSHOT_SCHEMA_VERSION {
            bail!(
                "gate snapshot {} has an incompatible manifest",
                manifest.key
            );
        }
        fs::create_dir_all(&self.target)?;
        let clone_started = Instant::now();
        self.clone_method = clone_tree(&selected.join("target"), &self.target)?.name();
        self.clone_ms = clone_started.elapsed().as_millis();
        let relocation_started = Instant::now();
        relocate_target(
            &self.target,
            &manifest,
            &self.workspace,
            &self.target,
            &self.resource_ledger,
        )?;
        stabilize_target_mtimes(&self.target)?;
        self.relocation_ms = relocation_started.elapsed().as_millis();
        self.write_target_marker(&manifest.key)?;
        self.restored = true;
        Ok(())
    }

    fn target_marker(&self) -> PathBuf {
        self.target.join("cargo-reapi/gate-state-v15")
    }

    fn target_marker_matches(&self, selected_key: &str) -> bool {
        fs::read_to_string(self.target_marker()).is_ok_and(|key| key.trim() == selected_key)
    }

    fn write_target_marker(&self, selected_key: &str) -> Result<()> {
        let marker = self.target_marker();
        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(marker, selected_key)?;
        Ok(())
    }

    fn unlock(&mut self) -> Result<()> {
        if let Some(lock) = self.lock.take() {
            FileExt::unlock(&lock).context("unlocking gate snapshot")?;
        }
        Ok(())
    }
}

impl Drop for GateSnapshot {
    fn drop(&mut self) {
        let _ = self.unlock();
    }
}

fn gate_key(
    cache: &Path,
    workspace: &Path,
    target: &Path,
    action_log: &Path,
    cargo_args: &[OsString],
    declared_inputs: &[PathBuf],
) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"cargo-reapi-gate-state-v15\0");
    hash_field(&mut hasher, std::env::consts::OS.as_bytes());
    hash_field(&mut hasher, std::env::consts::ARCH.as_bytes());
    let current_executable = std::env::current_exe()?.canonicalize()?;
    hash_field(&mut hasher, b"cargo-reapi-executable");
    hash_field(&mut hasher, &fs::read(&current_executable)?);
    let kernel_release = Command::new("uname").arg("-r").output()?;
    if !kernel_release.status.success() {
        bail!("uname -r failed while keying the host runtime");
    }
    hash_field(&mut hasher, b"kernel-release");
    hash_field(&mut hasher, &kernel_release.stdout);
    #[cfg(target_os = "macos")]
    {
        let os_build = Command::new("/usr/bin/sw_vers").output()?;
        if !os_build.status.success() {
            bail!("sw_vers failed while keying the macOS runtime");
        }
        hash_field(&mut hasher, b"macos-runtime");
        hash_field(&mut hasher, &os_build.stdout);
    }
    #[cfg(target_os = "linux")]
    {
        hash_field(&mut hasher, b"linux-runtime");
        hash_field(&mut hasher, &fs::read("/etc/os-release")?);
    }
    hash_tool_identity(&mut hasher, "cargo")?;
    hash_tool_identity(&mut hasher, "rustc")?;
    hash_field(&mut hasher, b"sandbox-provider");
    hash_field(
        &mut hasher,
        crate::hermetic::provider_identity_digest()?.as_bytes(),
    );
    hash_field(&mut hasher, b"sandbox-policy");
    let policy_identity = crate::hermetic::policy_identity_bytes(
        workspace,
        target,
        cache,
        action_log,
        declared_inputs,
    )?;
    hash_field(&mut hasher, &policy_identity);
    for tool in ["cc", "clang", "ld"] {
        if resolve_executable(tool).is_some() {
            hash_tool_identity(&mut hasher, tool)?;
        }
    }
    #[cfg(target_os = "macos")]
    for arguments in [
        &["--show-sdk-path"][..],
        &["--show-sdk-version"][..],
        &["--find", "clang"][..],
    ] {
        let output = Command::new("/usr/bin/xcrun").args(arguments).output()?;
        if !output.status.success() {
            bail!("xcrun {} failed while keying the SDK", arguments.join(" "));
        }
        hash_field(&mut hasher, &output.stdout);
        if arguments == ["--find", "clang"] {
            let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            hash_field(
                &mut hasher,
                &fs::read(&path)
                    .with_context(|| format!("hashing SDK linker identity {}", path.display()))?,
            );
        }
    }
    let mut gate_environment = std::env::vars_os()
        .filter(|(name, _)| is_gate_environment_key(&name.to_string_lossy()))
        .collect::<Vec<_>>();
    gate_environment.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, value) in gate_environment {
        hash_field(&mut hasher, name.to_string_lossy().as_bytes());
        let value = normalize_gate_text(&value.to_string_lossy(), workspace, target);
        hash_field(&mut hasher, value.as_bytes());
    }

    hash_tree(&mut hasher, workspace, "workspace", target, action_log)?;
    hash_cargo_configuration(&mut hasher, workspace)?;
    hash_external_path_dependencies(&mut hasher, cache, workspace, target, action_log)?;
    let mut declared_inputs = declared_inputs.to_vec();
    declared_inputs.sort();
    declared_inputs.dedup();
    for input in declared_inputs {
        let input = if input.is_absolute() {
            input
        } else {
            workspace.join(input)
        };
        if input.is_dir() {
            hash_tree(
                &mut hasher,
                &input,
                &format!("declared-input:{}", input.display()),
                target,
                action_log,
            )?;
        } else if input.is_file() {
            hash_field(&mut hasher, b"declared-input-file");
            hash_field(&mut hasher, input.to_string_lossy().as_bytes());
            hash_field(&mut hasher, &fs::read(&input)?);
        } else {
            bail!("declared input does not exist: {}", input.display());
        }
    }
    let state_key = format!("{:x}", hasher.finalize());
    let mut exact = Sha256::new();
    exact.update(b"cargo-reapi-exact-gate-v15\0");
    hash_field(&mut exact, state_key.as_bytes());
    for argument in cargo_args {
        hash_field(&mut exact, argument.to_string_lossy().as_bytes());
    }
    Ok(format!("{:x}", exact.finalize()))
}

fn hash_tree(
    hasher: &mut Sha256,
    root: &Path,
    logical_root: &str,
    target: &Path,
    action_log: &Path,
) -> Result<()> {
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.path() == root
                || !(entry.path().starts_with(target)
                    || entry.path() == action_log
                    || entry.file_name() == ".git"
                    || entry.file_name() == "target")
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path().to_path_buf());
    for entry in entries {
        if entry.path() == root || entry.file_type().is_dir() {
            continue;
        }
        let relative = entry.path().strip_prefix(root)?;
        hash_field(hasher, logical_root.as_bytes());
        hash_field(hasher, relative.to_string_lossy().as_bytes());
        let metadata = fs::symlink_metadata(entry.path())?;
        if entry.file_type().is_symlink() {
            hash_field(
                hasher,
                fs::read_link(entry.path())?.to_string_lossy().as_bytes(),
            );
        } else if entry.file_type().is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hash_field(hasher, &metadata.permissions().mode().to_le_bytes());
            }
            let mut file = File::open(entry.path())?;
            let mut buffer = vec![0_u8; 128 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
            }
        }
    }
    Ok(())
}

fn is_gate_environment_key(name: &str) -> bool {
    !matches!(
        name,
        "CARGO_REAPI_ACTION_LOG"
            | "CARGO_REAPI_BACKEND"
            | "CARGO_REAPI_CACHE_DIR"
            | "CARGO_REAPI_CLONE_TRACE"
            | "CARGO_REAPI_RUSTC_TRACE"
            | "CARGO_REAPI_TARGET_ROOT"
            | "CARGO_REAPI_WORKSPACE_ROOT"
            | "CARGO_TARGET_DIR"
    )
}

fn normalize_gate_text(value: &str, workspace: &Path, target: &Path) -> String {
    value
        .replace(&target.to_string_lossy().to_string(), "<target>")
        .replace(&workspace.to_string_lossy().to_string(), "<workspace>")
}

fn hash_cargo_configuration(hasher: &mut Sha256, workspace: &Path) -> Result<()> {
    for (distance, directory) in workspace.ancestors().enumerate() {
        for name in ["config", "config.toml"] {
            let path = directory.join(".cargo").join(name);
            if path.is_file() {
                hash_field(
                    hasher,
                    format!("cargo-config:ancestor={distance}:name={name}").as_bytes(),
                );
                hash_config_file(hasher, &path)?;
            }
        }
    }
    if let Some(cargo_home) = cargo_home() {
        for name in ["config", "config.toml"] {
            let path = cargo_home.join(name);
            if path.is_file() {
                hash_field(hasher, format!("cargo-home-config:name={name}").as_bytes());
                hash_config_file(hasher, &path)?;
            }
        }
    }
    Ok(())
}

fn hash_config_file(hasher: &mut Sha256, path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        hash_field(hasher, &metadata.permissions().mode().to_le_bytes());
    }
    if metadata.file_type().is_symlink() {
        hash_field(hasher, fs::read_link(path)?.to_string_lossy().as_bytes());
    }
    hash_field(hasher, &fs::read(path)?);
    Ok(())
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".cargo"))
        })
}

fn resolve_executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 && candidate.is_file() {
        return fs::canonicalize(candidate).ok();
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|root| root.join(name))
            .find(|candidate| candidate.is_file())
            .and_then(|candidate| fs::canonicalize(candidate).ok())
    })
}

fn hash_tool_identity(hasher: &mut Sha256, name: &str) -> Result<()> {
    let configured = (name == "rustc")
        .then(|| std::env::var_os("RUSTC"))
        .flatten()
        .map(PathBuf::from);
    let mut path = configured
        .or_else(|| resolve_executable(name))
        .with_context(|| format!("resolving tool identity for {name}"))?;
    if path
        .canonicalize()
        .ok()
        .and_then(|path| path.file_name().map(|file| file == "rustup"))
        .unwrap_or(false)
    {
        let output = Command::new("rustup").args(["which", name]).output()?;
        if output.status.success() {
            path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        }
    }
    path = fs::canonicalize(&path).unwrap_or(path);
    hash_field(hasher, b"tool-identity");
    hash_field(hasher, name.as_bytes());
    hash_field(hasher, path.to_string_lossy().as_bytes());
    hash_field(
        hasher,
        &fs::read(&path).with_context(|| format!("hashing tool identity {}", path.display()))?,
    );
    if name == "rustc"
        && let Some(real) = std::env::var_os("CARGO_REAPI_REAL_RUSTC").map(PathBuf::from)
        && real.is_file()
    {
        hash_field(hasher, b"observer-real-rustc");
        hash_field(hasher, &fs::read(real)?);
    }
    Ok(())
}

fn hash_external_path_dependencies(
    hasher: &mut Sha256,
    cache: &Path,
    workspace: &Path,
    target: &Path,
    action_log: &Path,
) -> Result<()> {
    let output = crate::query::cargo_metadata_output(workspace, cache, &[])
        .context("running cargo metadata for gate key")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed while keying gate snapshot: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut packages = metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|package| package["source"].is_null())
        .filter_map(|package| {
            let manifest = PathBuf::from(package["manifest_path"].as_str()?);
            let root = manifest.parent()?.to_path_buf();
            (!root.starts_with(workspace)).then(|| {
                (
                    package["id"].as_str().unwrap_or("local-package").to_owned(),
                    root,
                )
            })
        })
        .collect::<Vec<_>>();
    packages.sort_by(|left, right| left.0.cmp(&right.0));
    for (id, root) in packages {
        hash_tree(
            hasher,
            &root,
            &format!("path-dependency:{id}"),
            target,
            action_log,
        )?;
    }
    Ok(())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(value.len().to_le_bytes());
    hasher.update(value);
}

fn read_path_mappings(target: &Path) -> Result<Vec<RecordedPathMapping>> {
    let root = target.join("cargo-reapi/path-mappings");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut mappings: Vec<RecordedPathMapping> = Vec::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            mappings.push(serde_json::from_slice(&fs::read(path)?)?);
        }
    }
    mappings.sort_by(|left, right| left.label.cmp(&right.label));
    mappings.dedup_by(|left, right| left.label == right.label && left.actual == right.actual);
    Ok(mappings)
}

fn relocate_target(
    target_root: &Path,
    manifest: &GateSnapshotManifest,
    consumer_workspace: &Path,
    consumer_target: &Path,
    resource_ledger: &Path,
) -> Result<()> {
    let mut mappings = BTreeMap::new();
    mappings.insert(
        manifest.producer_workspace.clone(),
        consumer_workspace.to_path_buf(),
    );
    mappings.insert(
        manifest.producer_target.clone(),
        consumer_target.to_path_buf(),
    );
    for mapping in &manifest.path_mappings {
        let producer = PathBuf::from(&mapping.actual);
        if let Ok(relative) = producer.strip_prefix(&manifest.producer_workspace) {
            let consumer = consumer_workspace.join(relative);
            mappings.insert(producer, consumer);
        }
    }
    mappings.retain(|producer, consumer| producer != consumer);
    if mappings.is_empty() {
        return Ok(());
    }

    for relative in &manifest.relocation_files {
        let path = target_root.join(relative);
        if !path.is_file() {
            continue;
        }
        let metadata = fs::metadata(&path)?;
        let modified = metadata.modified()?;
        let accessed = metadata.accessed()?;
        let executable = is_executable(&metadata);
        let mut bytes = fs::read(&path)?;
        let original = bytes.clone();
        for (producer, consumer) in &mappings {
            if executable {
                let from = execution_slot(&producer.to_string_lossy())?;
                let to = execution_slot(&consumer.to_string_lossy())?;
                bytes = replace_bytes(&bytes, from.as_bytes(), to.as_bytes());
            }
            if is_cargo_text_metadata(&path) {
                bytes = replace_variable(
                    &bytes,
                    producer.to_string_lossy().as_bytes(),
                    consumer.to_string_lossy().as_bytes(),
                );
            }
            if is_cargo_binary_dep_info(&path) {
                bytes = replace_length_prefixed_paths(
                    &bytes,
                    producer.to_string_lossy().as_bytes(),
                    consumer.to_string_lossy().as_bytes(),
                )?;
            }
        }
        if bytes != original {
            fs::write(&path, bytes)?;
            if executable {
                resign(&path, resource_ledger)?;
            }
            OpenOptions::new().write(true).open(&path)?.set_times(
                fs::FileTimes::new()
                    .set_accessed(accessed)
                    .set_modified(modified),
            )?;
        }
    }
    Ok(())
}

fn build_relocation_index(
    target_root: &Path,
    producer_workspace: &Path,
    producer_target: &Path,
    recorded: &[RecordedPathMapping],
) -> Result<Vec<PathBuf>> {
    let mut roots = vec![
        producer_workspace.to_path_buf(),
        producer_target.to_path_buf(),
    ];
    roots.extend(recorded.iter().filter_map(|mapping| {
        let path = PathBuf::from(&mapping.actual);
        (path.starts_with(producer_workspace) || path == producer_target).then_some(path)
    }));
    roots.sort();
    roots.dedup();
    let raw_needles = roots
        .iter()
        .map(|root| root.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let slot_needles = raw_needles
        .iter()
        .map(|root| execution_slot(root))
        .collect::<Result<Vec<_>>>()?;

    let mut files = Vec::new();
    for entry in WalkDir::new(target_root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let executable = is_executable(&entry.metadata()?);
        let raw_candidate =
            is_cargo_text_metadata(entry.path()) || is_cargo_binary_dep_info(entry.path());
        if !executable && !raw_candidate {
            continue;
        }
        let bytes = fs::read(entry.path())?;
        let contains_raw = raw_candidate
            && raw_needles
                .iter()
                .any(|needle| contains_bytes(&bytes, needle.as_bytes()));
        let contains_slot = executable
            && slot_needles
                .iter()
                .any(|needle| contains_bytes(&bytes, needle.as_bytes()))
            && executable_runtime_contains(entry.path(), &bytes, &slot_needles).unwrap_or(true);
        if contains_raw || contains_slot {
            files.push(entry.path().strip_prefix(target_root)?.to_path_buf());
        }
    }
    files.sort();
    Ok(files)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn executable_runtime_contains(path: &Path, bytes: &[u8], needles: &[String]) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/otool")
            .arg("-l")
            .arg(path)
            .output()
            .with_context(|| format!("reading Mach-O sections from {}", path.display()))?;
        if !output.status.success() {
            bail!("otool could not inspect {}", path.display());
        }
        let text = String::from_utf8(output.stdout).context("otool returned non-UTF-8 output")?;
        let ranges = macho_runtime_ranges(&text, bytes.len());
        Ok(ranges.into_iter().any(|(start, end)| {
            needles
                .iter()
                .any(|needle| contains_bytes(&bytes[start..end], needle.as_bytes()))
        }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Ok(needles
            .iter()
            .any(|needle| contains_bytes(bytes, needle.as_bytes())))
    }
}

#[cfg(target_os = "macos")]
fn macho_runtime_ranges(output: &str, file_len: usize) -> Vec<(usize, usize)> {
    #[derive(Default)]
    struct Section {
        segment: String,
        offset: Option<usize>,
        size: Option<usize>,
    }

    fn append(section: &Section, ranges: &mut Vec<(usize, usize)>, file_len: usize) {
        let (Some(offset), Some(size)) = (section.offset, section.size) else {
            return;
        };
        if section.segment == "__DWARF" || offset >= file_len {
            return;
        }
        ranges.push((offset, offset.saturating_add(size).min(file_len)));
    }

    let mut ranges = Vec::new();
    let mut section = Section::default();
    let mut in_section = false;
    for line in output.lines().map(str::trim) {
        if line == "Section" {
            if in_section {
                append(&section, &mut ranges, file_len);
            }
            section = Section::default();
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(value) = line.strip_prefix("segname ") {
            value.clone_into(&mut section.segment);
        } else if let Some(value) = line.strip_prefix("offset ") {
            section.offset = value.parse().ok();
        } else if let Some(value) = line.strip_prefix("size ") {
            section.size = value
                .strip_prefix("0x")
                .and_then(|value| usize::from_str_radix(value, 16).ok())
                .or_else(|| value.parse().ok());
        }
    }
    if in_section {
        append(&section, &mut ranges, file_len);
    }
    if let Some(first_section) = ranges.iter().map(|(start, _)| *start).min() {
        ranges.push((0, first_section));
    }
    ranges
}

fn is_cargo_text_metadata(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "d" || extension == "json")
        || path
            .file_name()
            .is_some_and(|name| name == "output" || name == "stderr" || name == "root-output")
}

fn is_cargo_binary_dep_info(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".fingerprint")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("dep-"))
}

fn replace_length_prefixed_paths(
    bytes: &[u8],
    producer: &[u8],
    consumer: &[u8],
) -> Result<Vec<u8>> {
    let mut output = bytes.to_vec();
    let mut search_from = 0;
    while let Some(relative) = output[search_from..]
        .windows(producer.len())
        .position(|window| window == producer)
    {
        let position = search_from + relative;
        if position < 4 {
            bail!("Cargo dep-info path has no length prefix");
        }
        let length_offset = position - 4;
        let old_length = u32::from_le_bytes(
            output[length_offset..position]
                .try_into()
                .expect("four-byte length field"),
        ) as usize;
        if old_length < producer.len() || position + old_length > output.len() {
            bail!("Cargo dep-info path has an invalid length prefix");
        }
        let new_length = old_length - producer.len() + consumer.len();
        let new_length = u32::try_from(new_length).context("Cargo dep-info path is too long")?;
        output[length_offset..position].copy_from_slice(&new_length.to_le_bytes());
        output.splice(
            position..position + producer.len(),
            consumer.iter().copied(),
        );
        search_from = position + consumer.len();
    }
    Ok(output)
}

fn replace_variable(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut result = Vec::with_capacity(haystack.len());
    let mut offset = 0;
    while let Some(found) = haystack[offset..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let position = offset + found;
        result.extend_from_slice(&haystack[offset..position]);
        result.extend_from_slice(replacement);
        offset = position + needle.len();
    }
    result.extend_from_slice(&haystack[offset..]);
    result
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

fn resign(path: &Path, _resource_ledger: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _lease = ResourceLease::acquire_snapshot_signing_at(_resource_ledger)?;
        let output = Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(path)
            .output()
            .with_context(|| format!("re-signing restored executable {}", path.display()))?;
        if !output.status.success() {
            bail!(
                "codesign failed for restored executable {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = path;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloneMethod {
    CopyOnWrite,
    PortableCopy,
}

impl CloneMethod {
    fn name(self) -> &'static str {
        match self {
            Self::CopyOnWrite => "copy-on-write",
            Self::PortableCopy => "portable-copy",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClonePreference {
    Auto,
    Portable,
}

#[derive(Debug, Serialize)]
struct CloneTraceEvent {
    schema_version: u32,
    at_unix_ms: u128,
    pid: u32,
    platform_os: &'static str,
    source_location: &'static str,
    source: String,
    destination: String,
    preference: &'static str,
    attempted_primitive: &'static str,
    attempt_succeeded: bool,
    attempt_exit_code: Option<i32>,
    attempt_error: String,
    selected_method: &'static str,
}

fn clone_tree(source: &Path, destination: &Path) -> Result<CloneMethod> {
    clone_tree_with_preference(source, destination, ClonePreference::Auto)
}

fn clone_tree_with_preference(
    source: &Path,
    destination: &Path,
    preference: ClonePreference,
) -> Result<CloneMethod> {
    fs::create_dir_all(destination)?;
    if preference == ClonePreference::Portable {
        copy_tree_portable(source, destination)?;
        record_clone_trace(CloneTraceEvent {
            schema_version: 1,
            at_unix_ms: unix_ms(),
            pid: std::process::id(),
            platform_os: std::env::consts::OS,
            source_location: "src/gate.rs:clone_tree_with_preference",
            source: source.display().to_string(),
            destination: destination.display().to_string(),
            preference: "portable",
            attempted_primitive: "portable-copy",
            attempt_succeeded: true,
            attempt_exit_code: Some(0),
            attempt_error: String::new(),
            selected_method: CloneMethod::PortableCopy.name(),
        })?;
        return Ok(CloneMethod::PortableCopy);
    }
    #[cfg(target_os = "macos")]
    let primitive = "/bin/cp -cR";
    #[cfg(target_os = "macos")]
    let attempt = Command::new("/bin/cp")
        .arg("-cR")
        .arg(source.join("."))
        .arg(destination)
        .output();
    #[cfg(not(target_os = "macos"))]
    #[cfg(not(target_os = "windows"))]
    let primitive = "cp --reflink=always -a";
    #[cfg(not(target_os = "macos"))]
    #[cfg(not(target_os = "windows"))]
    let attempt = Command::new("cp")
        .args(["--reflink=always", "-a"])
        .arg(source.join("."))
        .arg(destination)
        .output();
    #[cfg(target_os = "windows")]
    let primitive = "windows-block-clone-unavailable";
    #[cfg(target_os = "windows")]
    let attempt: std::io::Result<std::process::Output> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Windows block cloning is unavailable through the portable standard library",
    ));
    let attempt_succeeded = attempt.as_ref().is_ok_and(|output| output.status.success());
    let attempt_exit_code = attempt
        .as_ref()
        .ok()
        .and_then(|output| output.status.code());
    let attempt_error = match &attempt {
        Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        Err(error) => error.to_string(),
    };
    if attempt_succeeded {
        record_clone_trace(CloneTraceEvent {
            schema_version: 1,
            at_unix_ms: unix_ms(),
            pid: std::process::id(),
            platform_os: std::env::consts::OS,
            source_location: "src/gate.rs:clone_tree_with_preference",
            source: source.display().to_string(),
            destination: destination.display().to_string(),
            preference: "auto",
            attempted_primitive: primitive,
            attempt_succeeded,
            attempt_exit_code,
            attempt_error,
            selected_method: CloneMethod::CopyOnWrite.name(),
        })?;
        return Ok(CloneMethod::CopyOnWrite);
    }
    copy_tree_portable(source, destination)?;
    record_clone_trace(CloneTraceEvent {
        schema_version: 1,
        at_unix_ms: unix_ms(),
        pid: std::process::id(),
        platform_os: std::env::consts::OS,
        source_location: "src/gate.rs:clone_tree_with_preference",
        source: source.display().to_string(),
        destination: destination.display().to_string(),
        preference: "auto",
        attempted_primitive: primitive,
        attempt_succeeded,
        attempt_exit_code,
        attempt_error,
        selected_method: CloneMethod::PortableCopy.name(),
    })?;
    Ok(CloneMethod::PortableCopy)
}

fn record_clone_trace(event: CloneTraceEvent) -> Result<()> {
    let Some(path) = std::env::var_os("CARGO_REAPI_CLONE_TRACE").map(PathBuf::from) else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    file.lock_exclusive()?;
    writeln!(file, "{}", serde_json::to_string(&event)?)?;
    file.sync_data()?;
    FileExt::unlock(&file)?;
    Ok(())
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn collect_observed_inputs(target: &Path) -> Result<Vec<ObservedGateInput>> {
    let mut inputs = Vec::new();
    for entry in WalkDir::new(target).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
            || !entry
                .path()
                .components()
                .any(|component| component.as_os_str() == ".fingerprint")
        {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&fs::read(entry.path())?)
        else {
            continue;
        };
        for local in value["local"].as_array().into_iter().flatten() {
            if let Some(changed) = local.get("RerunIfChanged") {
                for path in changed["paths"].as_array().into_iter().flatten() {
                    let Some(path) = path.as_str() else {
                        continue;
                    };
                    let path = PathBuf::from(path);
                    if path.is_absolute() {
                        let name = path.to_string_lossy().into_owned();
                        inputs.push(ObservedGateInput {
                            kind: "path".to_owned(),
                            sha256: observed_input_digest("path", &name),
                            name,
                        });
                    }
                }
            }
            if let Some(changed) = local.get("RerunIfEnvChanged")
                && let Some(name) = changed["var"].as_str()
            {
                inputs.push(ObservedGateInput {
                    kind: "environment".to_owned(),
                    name: name.to_owned(),
                    sha256: observed_input_digest("environment", name),
                });
            }
        }
    }
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

fn observed_input_digest(kind: &str, name: &str) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, kind.as_bytes());
    hash_field(&mut hasher, name.as_bytes());
    match kind {
        "environment" => match std::env::var_os(name) {
            Some(value) => hash_field(&mut hasher, value.to_string_lossy().as_bytes()),
            None => hash_field(&mut hasher, b"<absent>"),
        },
        "path" => {
            let path = Path::new(name);
            if path.is_file() {
                match fs::read(path) {
                    Ok(bytes) => hash_field(&mut hasher, &bytes),
                    Err(error) => hash_field(&mut hasher, error.to_string().as_bytes()),
                }
            } else if path.is_dir() {
                let mut entries = WalkDir::new(path)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_file())
                    .collect::<Vec<_>>();
                entries.sort_by_key(|entry| entry.path().to_path_buf());
                for entry in entries {
                    if let Ok(relative) = entry.path().strip_prefix(path) {
                        hash_field(&mut hasher, relative.to_string_lossy().as_bytes());
                    }
                    match fs::read(entry.path()) {
                        Ok(bytes) => hash_field(&mut hasher, &bytes),
                        Err(error) => hash_field(&mut hasher, error.to_string().as_bytes()),
                    }
                }
            } else {
                hash_field(&mut hasher, b"<missing>");
            }
        }
        _ => hash_field(&mut hasher, b"<unsupported>"),
    }
    format!("{:x}", hasher.finalize())
}

fn stabilize_target_mtimes(target: &Path) -> Result<()> {
    let timestamp = std::time::SystemTime::now();
    let times = fs::FileTimes::new()
        .set_accessed(timestamp)
        .set_modified(timestamp);
    for entry in WalkDir::new(target).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            OpenOptions::new()
                .write(true)
                .open(entry.path())?
                .set_times(times)?;
        }
    }
    Ok(())
}

fn copy_tree_portable(source: &Path, destination: &Path) -> Result<()> {
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        let output = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&output)?;
        } else if entry.file_type().is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &output)?;
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(fs::read_link(entry.path())?, &output)?;
        } else if entry.file_type().is_file() {
            fs::copy(entry.path(), &output)?;
            fs::set_permissions(&output, entry.metadata()?.permissions())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_snapshot_copy_is_a_complete_isolated_fallback() {
        let directory = tempfile::tempdir().expect("copy fixture");
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::create_dir_all(source.join("nested")).expect("source tree");
        fs::write(source.join("nested/artifact"), b"original").expect("source artifact");

        let method = clone_tree_with_preference(&source, &destination, ClonePreference::Portable)
            .expect("portable copy");
        assert_eq!(method, CloneMethod::PortableCopy);
        assert_eq!(
            fs::read(destination.join("nested/artifact")).expect("copied artifact"),
            b"original"
        );
        fs::write(destination.join("nested/artifact"), b"consumer").expect("mutate consumer");
        assert_eq!(
            fs::read(source.join("nested/artifact")).expect("source remains immutable"),
            b"original"
        );
    }

    #[test]
    fn variable_replacement_supports_different_length_worktrees() {
        assert_eq!(
            replace_variable(b"a /tmp/short b", b"/tmp/short", b"/tmp/a-long-consumer"),
            b"a /tmp/a-long-consumer b"
        );
    }

    #[test]
    fn relocates_length_prefixed_cargo_dep_info() {
        let producer = b"/tmp/p";
        let consumer = b"/tmp/a-long-consumer";
        let path = b"/tmp/p/assets/a.ron";
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&u32::try_from(path.len()).unwrap().to_le_bytes());
        encoded.extend_from_slice(path);
        encoded.push(0);
        let relocated = replace_length_prefixed_paths(&encoded, producer, consumer).unwrap();
        let expected = b"/tmp/a-long-consumer/assets/a.ron";
        assert_eq!(
            u32::from_le_bytes(relocated[..4].try_into().unwrap()) as usize,
            expected.len()
        );
        assert_eq!(&relocated[4..4 + expected.len()], expected);
    }

    #[test]
    fn gate_key_tracks_profile_and_target_overrides_without_tracking_worktree_target_path() {
        for name in [
            "CARGO_PROFILE_DEV_DEBUG",
            "CARGO_PROFILE_TEST_OPT_LEVEL",
            "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER",
            "CARGO_INCREMENTAL",
        ] {
            assert!(is_gate_environment_key(name), "missing gate input: {name}");
        }
        assert!(!is_gate_environment_key("CARGO_TARGET_DIR"));
        assert!(!is_gate_environment_key("CARGO_REAPI_ACTION_LOG"));
    }
}
