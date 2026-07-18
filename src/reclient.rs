use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::capture::{
    CaptureOptions, PreparedInvocation, PreparedOutput, prepare_invocation, record_invocation,
};
use crate::invocation::RustcInvocation;

pub struct ReclientOptions {
    rewrapper: PathBuf,
    cfg: PathBuf,
    staging_root: PathBuf,
    platform: String,
}

impl ReclientOptions {
    pub fn from_env() -> Result<Self> {
        Self {
            rewrapper: required_path("CARGO_REAPI_REWRAPPER")?,
            cfg: required_path("CARGO_REAPI_REWRAPPER_CFG")?,
            staging_root: required_path("CARGO_REAPI_RECLIENT_STAGING_DIR")?,
            platform: env::var("CARGO_REAPI_RECLIENT_PLATFORM")
                .context("CARGO_REAPI_RECLIENT_PLATFORM is required in REAPI mode")?,
        }
        .validate()
    }

    fn validate(self) -> Result<Self> {
        validate_platform_template(&self.platform)?;
        Ok(self)
    }
}

pub fn validate_platform_template(platform: &str) -> Result<()> {
    for placeholder in ["{os}", "{arch}", "{toolchain_sha256}"] {
        if !platform.contains(placeholder) {
            bail!("reclient platform template must contain {placeholder}");
        }
    }
    Ok(())
}

pub fn execute_reapi(
    invocation: &RustcInvocation,
    capture_options: &CaptureOptions,
    options: &ReclientOptions,
) -> Result<i32> {
    let prepared = prepare_invocation(invocation)?;
    if !prepared.remote_eligibility.eligible {
        let exit_code = invocation.execute()?;
        record_invocation(capture_options, &prepared, "local-ineligible", exit_code)?;
        return Ok(exit_code);
    }

    let stage = options.staging_root.join(&prepared.action_key);
    stage_inputs(&prepared, &stage)?;
    for output in &prepared.output_files {
        let staged = staged_path(&stage, &output.logical_path);
        if staged.is_file() {
            fs::remove_file(&staged)
                .with_context(|| format!("removing stale staged output {}", staged.display()))?;
        }
        if let Some(parent) = staged.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("creating staged output directory {}", parent.display())
            })?;
        }
    }

    let inputs = prepared
        .inputs
        .iter()
        .map(|input| staged_relative_path(&input.logical_path))
        .collect::<Vec<_>>();
    let outputs = prepared
        .output_files
        .iter()
        .map(|output| staged_relative_path(&output.logical_path))
        .collect::<Vec<_>>();
    let arguments = prepared
        .arguments
        .iter()
        .map(|argument| staged_argument(argument))
        .collect::<Vec<_>>();
    let action_environment = prepared
        .environment
        .iter()
        .map(|(name, value)| (name, staged_argument(value)))
        .collect::<BTreeMap<_, _>>();

    let platform = options
        .platform
        .replace("{os}", prepared.platform_os)
        .replace("{arch}", prepared.platform_arch)
        .replace("{toolchain_sha256}", &prepared.toolchain_sha256);
    let status = Command::new(&options.rewrapper)
        .current_dir(&stage)
        .arg(format!("--cfg={}", options.cfg.display()))
        .arg(format!("--exec_root={}", stage.display()))
        .arg(format!("--inputs={}", inputs.join(",")))
        .arg(format!("--output_files={}", outputs.join(",")))
        .arg("--labels=type=compile,compiler=rustc,lang=rust")
        .arg(format!("--platform={platform}"))
        .arg("--canonicalize_working_dir=true")
        .arg("--")
        .arg(&invocation.compiler)
        .args(arguments)
        .envs(action_environment)
        .status()
        .with_context(|| format!("starting rewrapper {}", options.rewrapper.display()))?;
    let exit_code = status.code().unwrap_or(1);
    if exit_code == 0 {
        for output in &prepared.output_files {
            materialize_output(&prepared, &stage, output)?;
        }
    }
    record_invocation(capture_options, &prepared, "reapi", exit_code)?;
    Ok(exit_code)
}

fn stage_inputs(prepared: &PreparedInvocation, stage: &Path) -> Result<()> {
    fs::create_dir_all(stage)
        .with_context(|| format!("creating reclient stage {}", stage.display()))?;
    for input in &prepared.inputs {
        let destination = staged_path(stage, &input.logical_path);
        let parent = destination
            .parent()
            .with_context(|| format!("staged input has no parent: {}", destination.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("creating staged input directory {}", parent.display()))?;
        fs::copy(&input.actual_path, &destination).with_context(|| {
            format!(
                "staging input {} as {}",
                input.actual_path.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn materialize_output(
    prepared: &PreparedInvocation,
    stage: &Path,
    output: &PreparedOutput,
) -> Result<()> {
    let staged = staged_path(stage, &output.logical_path);
    if !staged.is_file() {
        bail!(
            "rewrapper reported success without declared output {}",
            output.logical_path
        );
    }
    let parent = output
        .actual_path
        .parent()
        .with_context(|| format!("output has no parent: {}", output.actual_path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating output directory {}", parent.display()))?;
    let temporary = output
        .actual_path
        .with_extension(format!("reapi-{}", std::process::id()));
    if output
        .actual_path
        .extension()
        .is_some_and(|extension| extension == "d")
    {
        let mut bytes = fs::read(&staged)
            .with_context(|| format!("reading staged dep-info {}", staged.display()))?;
        for (label, actual) in &prepared.path_mappings {
            let staged_root = if label == "package" {
                stage.to_path_buf()
            } else {
                stage.join(label)
            };
            bytes = replace_bytes(
                &bytes,
                staged_root.to_string_lossy().as_bytes(),
                actual.as_bytes(),
            );
        }
        fs::write(&temporary, bytes)
            .with_context(|| format!("writing relocated dep-info {}", temporary.display()))?;
    } else {
        fs::copy(&staged, &temporary).with_context(|| {
            format!(
                "materializing reclient output {}",
                output.actual_path.display()
            )
        })?;
    }
    fs::rename(&temporary, &output.actual_path).with_context(|| {
        format!(
            "installing reclient output {}",
            output.actual_path.display()
        )
    })
}

fn staged_path(stage: &Path, logical: &str) -> PathBuf {
    stage.join(staged_relative_path(logical))
}

fn staged_relative_path(logical: &str) -> String {
    logical
        .strip_prefix("package/")
        .unwrap_or(logical)
        .trim_start_matches('/')
        .to_owned()
}

fn staged_argument(value: &str) -> String {
    if value == "package" {
        return ".".to_owned();
    }
    value
        .replace("package/", "")
        .replace("=package", "=.")
        .replace("@package", "@.")
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
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

fn required_path(name: &str) -> Result<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .with_context(|| format!("{name} is required in REAPI mode"))
}

#[cfg(test)]
mod tests {
    use super::{staged_argument, staged_relative_path, validate_platform_template};

    #[test]
    fn maps_package_root_to_reclient_execution_root() {
        assert_eq!(staged_relative_path("package/src/lib.rs"), "src/lib.rs");
        assert_eq!(
            staged_relative_path("target/debug/demo.rmeta"),
            "target/debug/demo.rmeta"
        );
        assert_eq!(staged_argument("package/src/lib.rs"), "src/lib.rs");
        assert_eq!(
            staged_argument("name=package/generated.rs"),
            "name=generated.rs"
        );
    }

    #[test]
    fn platform_contract_requires_host_and_toolchain_identity() {
        assert!(
            validate_platform_template(
                "OSFamily={os},Arch={arch},toolchain_sha256={toolchain_sha256}"
            )
            .is_ok()
        );
        assert!(validate_platform_template("OSFamily={os},Arch={arch}").is_err());
    }
}
