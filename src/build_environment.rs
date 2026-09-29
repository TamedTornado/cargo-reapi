//! Apply an explicit project environment before both planning and cache identity.
//! Values are changed in the real process, never merely omitted from a key.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const PROFILE_VARIABLE: &str = "CARGO_REAPI_BUILD_ENVIRONMENT";
const PROFILE_SCHEMA: &str = "cargo-reapi.build-environment/v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildEnvironment {
    schema: String,
    set: BTreeMap<String, String>,
    remove: Vec<String>,
}

impl BuildEnvironment {
    fn validate(&self) -> Result<()> {
        if self.schema != PROFILE_SCHEMA {
            bail!("unsupported build environment schema: {}", self.schema);
        }
        let mut removed = BTreeSet::new();
        for name in self.set.keys().chain(self.remove.iter()) {
            if name.is_empty() || name.contains(['=', '\0']) || name == PROFILE_VARIABLE {
                bail!("invalid or reserved build environment variable name: {name:?}");
            }
        }
        for (name, value) in &self.set {
            if value.contains('\0') {
                bail!("build environment value for {name} contains NUL");
            }
        }
        for name in &self.remove {
            if self.set.contains_key(name) || !removed.insert(name) {
                bail!("conflicting or duplicate build environment removal: {name}");
            }
        }
        Ok(())
    }

    fn apply(&self, command: &mut Command) {
        command.env_remove(PROFILE_VARIABLE).envs(&self.set);
        for name in &self.remove {
            command.env_remove(name);
        }
    }
}

/// Re-enter once with the effective environment before any Cargo/cache operation.
/// The selector is consumed; children inherit the actual resolved values. All
/// remaining environment, including arbitrary project variables, stays keyed.
pub fn enter_if_configured() -> Result<()> {
    let Some(path) = env::var_os(PROFILE_VARIABLE) else {
        return Ok(());
    };
    let path = std::path::PathBuf::from(path);
    let bytes =
        fs::read(&path).with_context(|| format!("reading build environment {}", path.display()))?;
    let profile: BuildEnvironment =
        serde_json::from_slice(&bytes).context("parsing explicit project build environment")?;
    profile.validate()?;

    let mut command = Command::new(env::current_exe().context("locating Cargo ReAPI")?);
    command.args(env::args_os().skip(1));
    profile.apply(&mut command);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("entering the explicit project build environment")
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .context("entering the project build environment")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_profiles_instead_of_falling_back_to_ambient_environment() {
        for input in [
            serde_json::json!({"schema":"old", "set":{}, "remove":[]}),
            serde_json::json!({"schema":PROFILE_SCHEMA, "set":{PROFILE_VARIABLE:"again"}, "remove":[]}),
            serde_json::json!({"schema":PROFILE_SCHEMA, "set":{"HOME":"x"}, "remove":["HOME"]}),
            serde_json::json!({"schema":PROFILE_SCHEMA, "set":{}, "remove":["HOME", "HOME"]}),
            serde_json::json!({"schema":PROFILE_SCHEMA, "set":{"A=B":"x"}, "remove":[]}),
            serde_json::json!({"schema":PROFILE_SCHEMA, "set":{"A":"\u{0}"}, "remove":[]}),
        ] {
            let profile: BuildEnvironment =
                serde_json::from_value(input).expect("structural profile");
            assert!(profile.validate().is_err());
        }
        assert!(
            serde_json::from_value::<BuildEnvironment>(serde_json::json!({
                "schema":PROFILE_SCHEMA, "set":{}, "remove":[], "ignore_for_cache":["HOME"]
            }))
            .is_err()
        );
    }
}
