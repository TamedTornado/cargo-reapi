//! The environment a cached compile runs with.
//!
//! A cached compile receives exactly the environment its action key covers.
//! Every other variable is removed from the compiler's environment before it
//! starts, so an unkeyed variable cannot influence the artifact. That includes
//! variables a proc macro reads through `std::env`, which rustc cannot track.
//!
//! The keyed environment is the union of:
//!
//! - the families rustc, Cargo, Clippy, the C toolchain and the native linker
//!   are documented to read ([`is_toolchain_environment`]);
//! - every variable Cargo or the sandbox added or changed relative to the
//!   driver's caller: Cargo's crate variables and build-script `rustc-env`
//!   values;
//! - every name Cargo's `[env]` configuration defines, whatever its value;
//! - the build script's `rustc-env` and `rerun-if-env-changed` declarations;
//! - the names listed in `CARGO_REAPI_DECLARED_ENV`;
//! - variables rustc recorded as `# env-dep:` dependencies of earlier compiles
//!   of the same crate.
//!
//! When the environment cannot be determined, the compile runs with and is
//! keyed on the whole environment instead.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::invocation::RustcInvocation;

/// The driver's own environment, one `NAME=sha256` line per variable.
/// Compiles compare against it to find what Cargo added.
pub const CALLER_ENVIRONMENT: &str = "CARGO_REAPI_CALLER_ENVIRONMENT";

/// Comma-separated names a compile must receive although nothing else marks
/// them: for example a variable a proc macro reads through `std::env`.
pub const DECLARED_ENVIRONMENT: &str = "CARGO_REAPI_DECLARED_ENV";

/// When set, each cached compile reports the variables removed from it.
pub const REPORT_STRIPPED_ENVIRONMENT: &str = "CARGO_REAPI_REPORT_STRIPPED_ENV";

/// The names Cargo's `[env]` configuration defines, which the driver reads
/// from the configuration Cargo loads: see [`config_record`].
pub const CONFIG_ENVIRONMENT: &str = "CARGO_REAPI_CONFIG_ENVIRONMENT";

/// Where the cache keeps the names compiles learned they read, one directory
/// per crate and one file per name.
const LEARNED_ENVIRONMENT_DIRECTORY: &str = "learned-environment";

/// Per-invocation plumbing that agent and container runtimes synthesize.
/// The driver removes it before Cargo starts, so build scripts never see it.
pub const RUNTIME_PLUMBING_ENVIRONMENT: [&str; 5] = [
    "CLAUDE_CODE_HOST_HTTP_PROXY_PORT",
    "CLAUDE_CODE_HOST_SOCKS_PROXY_PORT",
    "CODEX_THREAD_ID",
    "HOSTNAME",
    "SHLVL",
];

/// The audit observer, a compiler shim, reads these. The compiler identity
/// already covers the real compiler, and the trace directory only receives
/// observations, so they reach the compiler unkeyed.
const OBSERVER_ENVIRONMENT: [&str; 2] = ["CARGO_REAPI_REAL_RUSTC", "CARGO_REAPI_RUSTC_TRACE"];

/// Settings and records only the wrapper reads: where to log and cache,
/// scheduling capacity, and this module's own inputs. They never reach a
/// compiler and are never keyed.
const WRAPPER_PRIVATE: [&str; 11] = [
    CALLER_ENVIRONMENT,
    CONFIG_ENVIRONMENT,
    REPORT_STRIPPED_ENVIRONMENT,
    "CARGO_REAPI_ACTION_LOG",
    "CARGO_REAPI_BACKEND",
    "CARGO_REAPI_CACHE_DIR",
    "CARGO_REAPI_RESOURCE_CPU_CAPACITY",
    "CARGO_REAPI_RESOURCE_LEDGER",
    "CARGO_REAPI_RESOURCE_MEMORY_GIB_CAPACITY",
    "CARGO_REAPI_TARGET_ROOT",
    "CARGO_REAPI_WORKSPACE_ROOT",
];

/// Variable families read by rustc, Cargo, Clippy, the dynamic loader, the C
/// compiler driver and native linkers.
const TOOLCHAIN_PREFIXES: [&str; 12] = [
    "CARGO", "__CARGO", "RUST", "CLIPPY_", "LD_", "DYLD_", "GCC_", "COLLECT_", "CCC_", "CLANG_",
    "LC_", "MOLD_",
];

/// Individual variables with the same readers. `TARGET`, `HOST`, `PROFILE`,
/// `DEBUG`, `OPT_LEVEL` and `NUM_JOBS` belong to Cargo's build-script
/// interface; `OUT_DIR` is set for compiles too.
const TOOLCHAIN_NAMES: [&str; 30] = [
    "OUT_DIR",
    "TARGET",
    "HOST",
    "PROFILE",
    "DEBUG",
    "OPT_LEVEL",
    "NUM_JOBS",
    "PATH",
    "HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LANGUAGE",
    "LIBRARY_PATH",
    "COMPILER_PATH",
    "SOURCE_DATE_EPOCH",
    "CC",
    "CXX",
    "AR",
    "LD",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "SDKROOT",
    "DEVELOPER_DIR",
    "ZERO_AR_DATE",
    "MAKEFLAGS",
    "MFLAGS",
];

/// Executable and library search lists. Only the directories they resolve
/// through matter, so compiles receive, and are keyed by, their canonical
/// existing directories.
const SEARCH_PATH_ENVIRONMENT: [&str; 6] = [
    "PATH",
    "LD_LIBRARY_PATH",
    "LIBRARY_PATH",
    "COMPILER_PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
];

#[derive(Clone, Debug)]
pub enum CompileEnvironment {
    /// Only these variables, plus the audit observer's, reach the compiler,
    /// and only these are keyed.
    Keyed(BTreeSet<String>),
    /// The keyed environment could not be determined, so the compiler
    /// receives, and the key covers, the whole environment.
    Whole { reason: String },
}

impl CompileEnvironment {
    pub fn for_compile(invocation: &RustcInvocation, hints: Option<&EnvironmentHints>) -> Self {
        let Some(caller) = CallerEnvironment::from_env() else {
            return Self::whole("the cargo-reapi driver did not record its caller's environment");
        };
        if invocation.dep_info_file().is_none() {
            return Self::whole(
                "the compile emits no dep-info to reveal tracked environment reads",
            );
        }
        let build_script = match env::var_os("OUT_DIR") {
            Some(out_dir) => match build_script_environment(Path::new(&out_dir)) {
                Ok(names) => names,
                Err(error) => {
                    return Self::whole(&format!(
                        "the build script's declarations are unreadable: {error:#}"
                    ));
                }
            },
            None => BTreeSet::new(),
        };
        let configured = match ConfigEnvironment::from_env() {
            ConfigEnvironment::Known(names) => names,
            ConfigEnvironment::Unknown(reason) => return Self::whole(&reason),
        };
        let declared = declared_environment();
        let learned = hints.map(EnvironmentHints::load).unwrap_or_default();

        let mut names = BTreeSet::new();
        for (name, value) in env::vars_os() {
            let Some(name) = name.to_str() else {
                continue;
            };
            if WRAPPER_PRIVATE.contains(&name) || OBSERVER_ENVIRONMENT.contains(&name) {
                continue;
            }
            if is_toolchain_environment(name)
                || caller.changed(name, &value)
                || configured.contains(name)
                || build_script.contains(name)
                || declared.contains(name)
                || learned.contains(name)
            {
                names.insert(name.to_owned());
            }
        }
        Self::Keyed(names)
    }

    fn whole(reason: &str) -> Self {
        Self::Whole {
            reason: reason.to_owned(),
        }
    }

    /// Whether the compiler receives this variable.
    pub fn passes(&self, name: &OsStr) -> bool {
        let Some(name) = name.to_str() else {
            return matches!(self, Self::Whole { .. });
        };
        if WRAPPER_PRIVATE.contains(&name) {
            return false;
        }
        match self {
            Self::Keyed(names) => names.contains(name) || OBSERVER_ENVIRONMENT.contains(&name),
            Self::Whole { .. } => true,
        }
    }

    fn keys(&self, name: &str) -> bool {
        if WRAPPER_PRIVATE.contains(&name) || OBSERVER_ENVIRONMENT.contains(&name) {
            return false;
        }
        match self {
            Self::Keyed(names) => names.contains(name),
            Self::Whole { .. } => true,
        }
    }

    /// Passes more variables to later executions of this compile. Returns
    /// whether the compiler's environment changed.
    pub fn pass(&mut self, extra: &BTreeSet<String>) -> bool {
        let Self::Keyed(names) = self else {
            return false;
        };
        let before = names.len();
        names.extend(
            extra
                .iter()
                .filter(|name| !WRAPPER_PRIVATE.contains(&name.as_str()))
                .cloned(),
        );
        names.len() != before
    }

    /// Tracked variables the caller set that this environment withholds but
    /// could pass. Wrapper settings are never the caller's to give a
    /// compile, so a compile that reads one sees it unset, as under Cargo.
    pub fn withheld(&self, tracked: &BTreeSet<String>) -> BTreeSet<String> {
        tracked
            .iter()
            .filter(|name| {
                env::var_os(name).is_some()
                    && !WRAPPER_PRIVATE.contains(&name.as_str())
                    && !self.passes(OsStr::new(name))
            })
            .cloned()
            .collect()
    }

    /// The value the compiler receives for a variable it is passed. A search
    /// list is reduced to the directories it resolves through, which is also
    /// what its key covers.
    pub fn compiler_value(name: &OsStr, value: &OsStr) -> OsString {
        if name
            .to_str()
            .is_some_and(|name| SEARCH_PATH_ENVIRONMENT.contains(&name))
        {
            canonical_search_path(value)
        } else {
            value.to_owned()
        }
    }

    /// Variables present in this process that the compiler does not receive.
    pub fn stripped(&self) -> Vec<String> {
        let mut stripped = env::vars_os()
            .filter(|(name, _)| !self.passes(name))
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .filter(|name| !WRAPPER_PRIVATE.contains(&name.as_str()))
            .collect::<Vec<_>>();
        stripped.sort();
        stripped
    }

    /// Digests of every keyed value, after `normalize` maps worktree roots to
    /// their logical names.
    pub fn keyed_values(&self, normalize: impl Fn(&str) -> String) -> BTreeMap<String, String> {
        env::vars_os()
            .filter_map(|(name, value)| {
                let value = Self::compiler_value(&name, &value);
                let name = name.to_string_lossy().into_owned();
                if !self.keys(&name) {
                    return None;
                }
                let normalized = normalize(&value.to_string_lossy());
                Some((name, format!("sha256:{:x}", Sha256::digest(normalized))))
            })
            .collect()
    }

    pub fn report(&self, crate_name: Option<&str>) {
        if env::var_os(REPORT_STRIPPED_ENVIRONMENT).is_none() {
            return;
        }
        let crate_name = crate_name.unwrap_or("compiler query");
        match self {
            Self::Keyed(_) => eprintln!(
                "cargo-reapi: {crate_name} compiles without: {}",
                self.stripped().join(" ")
            ),
            Self::Whole { reason } => eprintln!(
                "cargo-reapi: {crate_name} compiles with the whole environment because {reason}"
            ),
        }
    }
}

/// Whether rustc, Cargo, Clippy, the dynamic loader, the C toolchain or a
/// native linker is documented to read this variable.
pub fn is_toolchain_environment(name: &str) -> bool {
    TOOLCHAIN_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || TOOLCHAIN_NAMES.contains(&name)
        || name.ends_with("_DEPLOYMENT_TARGET")
}

/// Encodes this process's environment for [`CALLER_ENVIRONMENT`].
pub fn caller_record() -> OsString {
    let mut lines = env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.to_str()?;
            (!name.contains(['=', '\n']) && !WRAPPER_PRIVATE.contains(&name))
                .then(|| format!("{name}={}", value_digest(&value)))
        })
        .collect::<Vec<_>>();
    lines.sort();
    OsString::from(lines.join("\n"))
}

struct CallerEnvironment {
    digests: BTreeMap<String, String>,
}

impl CallerEnvironment {
    fn from_env() -> Option<Self> {
        let record = env::var(CALLER_ENVIRONMENT).ok()?;
        let digests = record
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, digest)| (name.to_owned(), digest.to_owned()))
            .collect();
        Some(Self { digests })
    }

    /// Whether Cargo or the sandbox added this variable or changed its value.
    fn changed(&self, name: &str, value: &OsStr) -> bool {
        self.digests
            .get(name)
            .is_none_or(|digest| *digest != value_digest(value))
    }
}

fn value_digest(value: &OsStr) -> String {
    format!("{:x}", Sha256::digest(value.as_encoded_bytes()))
}

fn declared_environment() -> BTreeSet<String> {
    env::var(DECLARED_ENVIRONMENT)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Names a build script passes to its crate's compile (`rustc-env`) or
/// declares it read (`rerun-if-env-changed`). Cargo records the script's
/// output beside its `OUT_DIR`.
fn build_script_environment(out_dir: &Path) -> Result<BTreeSet<String>> {
    let output = out_dir
        .parent()
        .context("OUT_DIR has no parent build directory")?
        .join("output");
    let text = fs::read(&output)
        .with_context(|| format!("reading build-script output {}", output.display()))?;

    let mut names = BTreeSet::new();
    for line in String::from_utf8_lossy(&text).lines() {
        let Some(instruction) = line
            .strip_prefix("cargo::")
            .or_else(|| line.strip_prefix("cargo:"))
        else {
            continue;
        };
        if let Some(name) = instruction.strip_prefix("rerun-if-env-changed=") {
            names.insert(name.trim().to_owned());
        } else if let Some(assignment) = instruction.strip_prefix("rustc-env=")
            && let Some((name, _)) = assignment.split_once('=')
        {
            names.insert(name.to_owned());
        }
    }
    Ok(names)
}

/// Names rustc recorded as `# env-dep:` lines: `env!`, `option_env!` and
/// proc macros' tracked reads, whether or not the variable was set.
pub fn tracked_environment(dep_info: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(dep_info)
        .lines()
        .filter_map(|line| line.strip_prefix("# env-dep:"))
        .map(|entry| unescape_dep_info(entry.split_once('=').map_or(entry, |(name, _)| name)))
        .collect()
}

fn unescape_dep_info(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            unescaped.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => unescaped.push('\n'),
            Some('r') => unescaped.push('\r'),
            Some(other) => unescaped.push(other),
            None => unescaped.push('\\'),
        }
    }
    unescaped
}

/// A search list reduced to what it resolves through: each absolute entry
/// becomes its canonical directory, entries that are not directories are
/// dropped, and only the first occurrence of a directory is kept. Lookup
/// takes the first match, so the result finds exactly the same files.
/// Relative and empty entries resolve against the working directory, which
/// the action key already covers, and stay as written.
///
/// The compiler receives this value, not the caller's spelling, so whatever
/// reads the variable sees exactly what the key covers.
fn canonical_search_path(value: &OsStr) -> OsString {
    let mut seen = BTreeSet::new();
    let mut directories = Vec::new();
    for entry in env::split_paths(value) {
        let directory = if entry.as_os_str().is_empty() || entry.is_relative() {
            Some(entry)
        } else {
            fs::canonicalize(&entry)
                .ok()
                .filter(|directory| directory.is_dir())
        };
        if let Some(directory) = directory
            && seen.insert(directory.clone())
        {
            directories.push(directory);
        }
    }
    env::join_paths(directories).unwrap_or_else(|_| value.to_owned())
}

/// Tracked variables learned from earlier compiles of one crate, shared
/// through the cache so later compiles pass them from the start.
///
/// Each learned name is its own file, created once and never rewritten, so
/// compiles learning different names at the same time cannot lose each
/// other's.
pub struct EnvironmentHints {
    directory: PathBuf,
}

impl EnvironmentHints {
    pub fn new(cache_root: &Path, invocation: &RustcInvocation) -> Option<Self> {
        let crate_name = invocation.crate_name()?;
        let mut identity = Sha256::new();
        for part in [
            crate_name.to_owned(),
            env::var_os("CARGO_PKG_NAME").unwrap_or_default(),
            env::var_os("CARGO_PKG_VERSION").unwrap_or_default(),
        ] {
            identity.update(part.as_encoded_bytes());
            identity.update([0]);
        }
        Some(Self {
            directory: cache_root
                .join(LEARNED_ENVIRONMENT_DIRECTORY)
                .join(format!("{:x}", identity.finalize())),
        })
    }

    fn load(&self) -> BTreeSet<String> {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return BTreeSet::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| decode_name(&entry.file_name()))
            .collect()
    }

    pub fn record(&self, names: &BTreeSet<String>) -> Result<()> {
        fs::create_dir_all(&self.directory).with_context(|| {
            format!(
                "creating learned environment directory {}",
                self.directory.display()
            )
        })?;
        for name in names {
            let path = self.directory.join(encode_name(name));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("recording learned name {}", path.display()));
                }
            }
        }
        Ok(())
    }
}

/// A digest of every name learned for every crate. Learning changes how
/// later compiles run, so state built before it must not be reused after.
pub fn learned_environment_digest(cache_root: &Path) -> Result<String> {
    let root = cache_root.join(LEARNED_ENVIRONMENT_DIRECTORY);
    let mut entries = Vec::new();
    if root.is_dir() {
        for entry in walkdir::WalkDir::new(&root).min_depth(2).max_depth(2) {
            let entry = entry.context("listing learned environment")?;
            let relative = entry
                .path()
                .strip_prefix(&root)
                .context("learned name outside its directory")?;
            entries.push(relative.to_string_lossy().into_owned());
        }
    }
    entries.sort();
    Ok(format!("{:x}", Sha256::digest(entries.join("\n"))))
}

/// File names are hex, since a variable name may hold any character but `=`.
fn encode_name(name: &str) -> String {
    use std::fmt::Write;

    name.bytes().fold(String::new(), |mut encoded, byte| {
        let _ = write!(encoded, "{byte:02x}");
        encoded
    })
}

fn decode_name(file_name: &OsStr) -> Option<String> {
    let text = file_name.to_str()?;
    if text.len() % 2 != 0 {
        return None;
    }
    let bytes = (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}

/// The names Cargo's `[env]` configuration defines, or why they could not be
/// determined.
enum ConfigEnvironment {
    Known(BTreeSet<String>),
    Unknown(String),
}

impl ConfigEnvironment {
    fn from_env() -> Self {
        let Some(record) = env::var_os(CONFIG_ENVIRONMENT) else {
            return Self::Unknown(
                "the cargo-reapi driver did not record Cargo's [env] configuration".to_owned(),
            );
        };
        let record = record.to_string_lossy();
        let mut lines = record.lines();
        match lines.next() {
            Some("known") => Self::Known(lines.map(str::to_owned).collect()),
            _ => Self::Unknown(lines.collect::<Vec<_>>().join(" ")),
        }
    }
}

/// Encodes, for [`CONFIG_ENVIRONMENT`], the names that Cargo's `[env]`
/// configuration defines for a Cargo run in `cwd` with `cargo_args`.
///
/// Cargo sets those variables for every compile, or leaves the caller's
/// value when the entry is not forced. Either way plain Cargo's compile
/// receives them, so a cached compile must too, whatever their values. When
/// the configuration cannot be read completely, compiles receive the whole
/// environment instead.
pub fn config_record(cwd: &Path, cargo_args: &[OsString]) -> OsString {
    let record = match configured_names(cwd, cargo_args) {
        Ok(names) => std::iter::once("known".to_owned())
            .chain(names)
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => format!("unknown\nCargo's [env] configuration is unreadable: {error:#}"),
    };
    OsString::from(record)
}

/// Every name an `[env]` table in Cargo's configuration defines.
fn configured_names(cwd: &Path, cargo_args: &[OsString]) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for layer in crate::cargo_config::layers(cwd, cargo_args)? {
        if let Some(environment) = layer.table.get("env") {
            let environment = environment
                .as_table()
                .with_context(|| format!("[env] in {} is not a table", layer.origin))?;
            names.extend(environment.keys().cloned());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsStr;

    use super::{
        CompileEnvironment, canonical_search_path, is_toolchain_environment, tracked_environment,
    };

    #[test]
    fn documented_toolchain_variables_are_keyed_and_shell_bookkeeping_is_not() {
        for name in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_PKG_VERSION",
            "RUSTC_BOOTSTRAP",
            "CLIPPY_ARGS",
            "PATH",
            "LD_LIBRARY_PATH",
            "LIBRARY_PATH",
            "MACOSX_DEPLOYMENT_TARGET",
            "OUT_DIR",
            "TMPDIR",
        ] {
            assert!(is_toolchain_environment(name), "{name} must be keyed");
        }
        for name in [
            "OLDPWD",
            "PWD",
            "_",
            "SHLVL",
            "TERM",
            "CLAUDE_CODE_SESSION_ID",
        ] {
            assert!(!is_toolchain_environment(name), "{name} is not toolchain");
        }
    }

    #[test]
    fn dep_info_reports_set_and_unset_tracked_variables() {
        let dep_info = b"out/a.d: a.rs\n\na.rs:\n\n# env-dep:ABSENT\n# env-dep:PRESENT=x=y\n# env-dep:ESCAPED\\nNAME=v\\\\\n";
        assert_eq!(
            tracked_environment(dep_info),
            BTreeSet::from([
                "ABSENT".to_owned(),
                "ESCAPED\nNAME".to_owned(),
                "PRESENT".to_owned(),
            ])
        );
    }

    #[test]
    fn search_paths_key_symlinked_and_duplicate_directories_once() {
        let root = tempfile::tempdir().expect("search path root");
        let real = root.path().join("real");
        std::fs::create_dir(&real).expect("real directory");
        let link = root.path().join("per-shell-link");
        std::os::unix::fs::symlink(&real, &link).expect("per-shell symlink");
        let missing = root.path().join("missing");

        let through_link = std::env::join_paths([&link, &missing, &real]).unwrap();
        let direct = std::env::join_paths([&real]).unwrap();
        assert_eq!(
            canonical_search_path(&through_link),
            canonical_search_path(&direct)
        );

        let other = root.path().join("other");
        std::fs::create_dir(&other).expect("other directory");
        let reordered = std::env::join_paths([&other, &real]).unwrap();
        assert_ne!(
            canonical_search_path(&reordered),
            canonical_search_path(&direct),
            "a different first directory can resolve different executables"
        );
    }

    #[test]
    fn keyed_environment_never_passes_wrapper_records() {
        let keyed = CompileEnvironment::Keyed(BTreeSet::from(["PATH".to_owned()]));
        assert!(keyed.passes(OsStr::new("PATH")));
        assert!(keyed.passes(OsStr::new("CARGO_REAPI_RUSTC_TRACE")));
        assert!(!keyed.passes(OsStr::new("CARGO_REAPI_ACTION_LOG")));
        assert!(!keyed.passes(OsStr::new("OLDPWD")));
        assert!(!keyed.passes(OsStr::new("CARGO_REAPI_CALLER_ENVIRONMENT")));

        let whole = CompileEnvironment::Whole {
            reason: "test".to_owned(),
        };
        assert!(whole.passes(OsStr::new("OLDPWD")));
        assert!(!whole.passes(OsStr::new("CARGO_REAPI_CALLER_ENVIRONMENT")));
    }
}
