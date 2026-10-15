use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::environment::CompileEnvironment;
use crate::relocation::execution_slot;

#[derive(Debug)]
pub struct RustcInvocation {
    pub compiler: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

impl RustcInvocation {
    pub fn looks_like_wrapper(args: &[OsString]) -> bool {
        args.get(1).is_some_and(|value| {
            value != "reapi"
                && value != "contract"
                && value != "prove"
                && !is_cargo_driver_command(value)
                && !value.to_string_lossy().starts_with('-')
        })
    }

    pub fn parse(args: Vec<OsString>) -> Result<Self> {
        let compiler = args.get(1).context("missing rustc path")?;
        Ok(Self {
            compiler: PathBuf::from(compiler),
            args: args.into_iter().skip(2).collect(),
            cwd: std::env::current_dir().context("reading wrapper working directory")?,
        })
    }

    /// Runs the compiler with the caller's whole environment, as Cargo would.
    pub fn execute(&self) -> Result<i32> {
        self.execute_in(None)
    }

    /// Runs the compiler with `environment` alone, or with the caller's whole
    /// environment when it is `None`.
    pub fn execute_in(&self, environment: Option<&CompileEnvironment>) -> Result<i32> {
        self.record_physical_compiler_observation()?;
        let mut command = Command::new(&self.compiler);
        command.args(&self.args);
        apply_compiler_environment(&mut command, &self.compiler, environment)?;
        let status = command
            .status()
            .with_context(|| format!("executing {}", self.compiler.display()))?;
        Ok(status.code().unwrap_or(1))
    }

    /// Record only compiler processes that cargo-reapi is actually about to
    /// execute. Cache hits never reach this method, so this trace cannot turn
    /// a restored action into a false physical-compiler observation.
    fn record_physical_compiler_observation(&self) -> Result<()> {
        let Some(trace_root) = std::env::var_os("CARGO_REAPI_RUSTC_TRACE").map(PathBuf::from)
        else {
            return Ok(());
        };
        let mut kind = "compile";
        let mut crate_name = None;
        let mut arguments = self.args.iter();
        while let Some(argument) = arguments.next() {
            if argument == "--crate-name" {
                crate_name = arguments
                    .next()
                    .map(|value| value.to_string_lossy().into_owned());
            }
            if argument == "--print" || argument.to_string_lossy().starts_with("--print=") {
                kind = "query";
            }
        }
        if crate_name.is_none() {
            kind = "query";
        }
        fs::create_dir_all(&trace_root).with_context(|| {
            format!("creating compiler trace directory {}", trace_root.display())
        })?;
        let mut output =
            create_observation_file(&trace_root, crate_name.as_deref().unwrap_or("control"))?;
        writeln!(output, "kind={kind}")?;
        writeln!(
            output,
            "crate_name={}",
            crate_name.as_deref().unwrap_or("control")
        )?;
        writeln!(output, "cwd={}", self.cwd.display())?;
        Ok(())
    }

    pub fn crate_name(&self) -> Option<&OsStr> {
        value_after(&self.args, "--crate-name")
    }

    pub fn is_link_action(&self) -> bool {
        emit_values(&self.args)
            .iter()
            .any(|(kind, _)| kind == "link")
    }

    pub fn requires_native_linker(&self) -> bool {
        if !self.is_link_action() {
            return false;
        }
        let crate_types = option_values(&self.args, "--crate-type");
        crate_types.is_empty()
            || crate_types
                .iter()
                .any(|crate_type| !matches!(crate_type.as_str(), "lib" | "rlib"))
    }

    pub fn add_stable_path_remapping(&mut self) -> Result<()> {
        if self.crate_name().is_none() || self.out_dir().is_none() {
            return Ok(());
        }
        let mut mappings = Vec::new();
        for (name, logical) in [
            ("CARGO_MANIFEST_DIR", "/__cargo_reapi__/package"),
            ("CARGO_REAPI_TARGET_ROOT", "/__cargo_reapi__/target"),
            ("CARGO_REAPI_WORKSPACE_ROOT", "/__cargo_reapi__/workspace"),
        ] {
            if let Some(path) = std::env::var_os(name) {
                mappings.push((PathBuf::from(path), logical));
            }
        }
        if let Some(toolchain) = self.compiler.parent().and_then(Path::parent) {
            mappings.push((toolchain.to_path_buf(), "/__cargo_reapi__/toolchain"));
        }
        mappings.sort_by(|(left_path, left_label), (right_path, right_label)| {
            right_path
                .components()
                .count()
                .cmp(&left_path.components().count())
                .then_with(|| left_label.cmp(right_label))
        });
        mappings.dedup_by(|(left, _), (right, _)| left == right);
        for (actual, _logical) in mappings {
            let destination = execution_slot(&actual.to_string_lossy())?;
            self.args.push(OsString::from("--remap-path-prefix"));
            self.args.push(OsString::from(format!(
                "{}={destination}",
                actual.to_string_lossy()
            )));
        }
        Ok(())
    }

    pub fn out_dir(&self) -> Option<PathBuf> {
        value_after(&self.args, "--out-dir").map(PathBuf::from)
    }

    /// The dependency file rustc writes, when the compile asks for one.
    pub fn dep_info_file(&self) -> Option<PathBuf> {
        let (_, explicit_path) = emit_values(&self.args)
            .into_iter()
            .find(|(kind, _)| kind == "dep-info")?;
        if let Some(path) = explicit_path {
            return Some(PathBuf::from(path));
        }
        let crate_name = self.crate_name()?.to_string_lossy();
        let extra_filename = codegen_value(&self.args, "extra-filename").unwrap_or_default();
        Some(
            self.out_dir()?
                .join(format!("{crate_name}{extra_filename}.d")),
        )
    }

    pub fn output_files(&self) -> Result<Vec<PathBuf>> {
        if let Some(output) = value_after(&self.args, "-o") {
            return Ok(vec![PathBuf::from(output)]);
        }
        let Some(out_dir) = self.out_dir() else {
            return Ok(Vec::new());
        };
        let crate_name = self
            .crate_name()
            .map(|value| value.to_string_lossy().into_owned())
            .context("rustc action has --out-dir but no --crate-name")?;
        let extra_filename = codegen_value(&self.args, "extra-filename").unwrap_or_default();
        let emits = emit_values(&self.args);
        let printed = Command::new(&self.compiler)
            .args(&self.args)
            .args(["--print", "file-names"])
            .output()
            .context("asking rustc for its output filename")?;
        if !printed.status.success() {
            bail!(
                "rustc --print file-names failed: {}",
                String::from_utf8_lossy(&printed.stderr).trim()
            )
        }
        // One linked file per crate type: a `cdylib` and `rlib` library links
        // both a `.so` and an `.rlib`.
        let printed = String::from_utf8(printed.stdout)
            .context("rustc returned a non-UTF-8 output filename")?;
        let link_names: Vec<&str> = printed.lines().filter(|line| !line.is_empty()).collect();
        if link_names.is_empty() {
            bail!("rustc returned no output filename");
        }
        let mut outputs = Vec::new();
        for (kind, explicit_path) in emits {
            if let Some(path) = explicit_path {
                outputs.push(PathBuf::from(path));
                continue;
            }
            match kind.as_str() {
                "link" => outputs.extend(link_names.iter().map(|name| out_dir.join(name))),
                "metadata" => {
                    outputs.push(out_dir.join(format!("lib{crate_name}{extra_filename}.rmeta")));
                }
                "dep-info" => outputs.push(out_dir.join(format!("{crate_name}{extra_filename}.d"))),
                "obj" => outputs.push(out_dir.join(format!("{crate_name}{extra_filename}.o"))),
                _ => {}
            }
        }
        outputs.sort();
        outputs.dedup();
        Ok(outputs)
    }
}

fn create_observation_file(trace_root: &Path, crate_name: &str) -> Result<File> {
    for sequence in 0..u32::MAX {
        let record = trace_root.join(format!(
            "rustc-observation.{}.{}.{}",
            std::process::id(),
            crate_name,
            sequence
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&record)
        {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("creating compiler observation {}", record.display())
                });
            }
        }
    }
    bail!(
        "compiler observation namespace exhausted in {}",
        trace_root.display()
    )
}

fn is_cargo_driver_command(value: &OsStr) -> bool {
    matches!(
        value.to_str(),
        Some(
            "bench"
                | "build"
                | "check"
                | "clean"
                | "clippy"
                | "doc"
                | "fetch"
                | "fix"
                | "fmt"
                | "install"
                | "metadata"
                | "package"
                | "publish"
                | "run"
                | "test"
                | "tree"
                | "update"
        )
    )
}

/// Gives the compiler its environment: `environment` alone when present,
/// otherwise everything inherited. Values under a worktree, target or
/// toolchain root are relocated into their execution slots either way.
fn apply_compiler_environment(
    command: &mut Command,
    compiler: &Path,
    environment: Option<&CompileEnvironment>,
) -> Result<()> {
    if let Some(environment) = environment {
        command.env_clear();
        command.envs(
            std::env::vars_os()
                .filter(|(name, _)| environment.passes(name))
                .map(|(name, value)| {
                    let value = CompileEnvironment::compiler_value(&name, &value);
                    (name, value)
                }),
        );
    }
    apply_relocation_environment(command, compiler, environment)?;
    apply_observer_environment(command, compiler);
    Ok(())
}

/// The marker line the audit observer shim carries.
const OBSERVER_MARKER: &[u8] = b"cargo-reapi-rustc-observer";

/// The first bytes of a file, where the shim's marker line is.
fn observer_head(compiler: &Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let mut head = Vec::new();
    std::fs::File::open(compiler)
        .ok()?
        .take(1024)
        .read_to_end(&mut head)
        .ok()?;
    Some(head)
}

/// Gives the audit variables to a compiler that is the observer shim and
/// removes them from every other: a crate must not read them
/// (`option_env!`), and they are not keyed. The shim is recognized by its own
/// marker line, never inferred from the variables being set.
fn apply_observer_environment(command: &mut Command, compiler: &Path) {
    let is_shim = observer_head(compiler).is_some_and(|head| {
        head.starts_with(b"#!")
            && head
                .windows(OBSERVER_MARKER.len())
                .any(|part| part == OBSERVER_MARKER)
    });
    for name in crate::environment::OBSERVER_ENVIRONMENT {
        match std::env::var_os(name) {
            Some(value) if is_shim => {
                command.env(name, value);
            }
            _ => {
                command.env_remove(name);
            }
        }
    }
}

fn apply_relocation_environment(
    command: &mut Command,
    compiler: &Path,
    environment: Option<&CompileEnvironment>,
) -> Result<()> {
    let mut roots = Vec::new();
    for name in [
        "CARGO_MANIFEST_DIR",
        "CARGO_REAPI_TARGET_ROOT",
        "CARGO_REAPI_WORKSPACE_ROOT",
    ] {
        if let Some(path) = std::env::var_os(name) {
            roots.push(PathBuf::from(path));
        }
    }
    if let Some(toolchain) = compiler.parent().and_then(Path::parent) {
        roots.push(toolchain.to_path_buf());
    }
    roots.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    roots.dedup();

    for (name, value) in std::env::vars_os() {
        let value = match environment {
            Some(environment) if !environment.passes(&name) => continue,
            Some(_) => CompileEnvironment::compiler_value(&name, &value),
            None => value,
        };
        if let Some(relocated) = relocated_environment_value(&value, &roots)? {
            command.env(name, relocated);
        }
    }
    Ok(())
}

fn relocated_environment_value(value: &OsStr, roots: &[PathBuf]) -> Result<Option<OsString>> {
    let Some(value) = value.to_str() else {
        return relocated_environment_path(value, roots);
    };
    let mut matches = Vec::new();
    for root in roots {
        if let Some(root) = root.to_str() {
            matches.push((root, execution_slot(root)?));
        }
    }
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    let mut changed = false;
    while !remaining.is_empty() {
        let Some((offset, root, slot)) = matches
            .iter()
            .filter_map(|(root, slot)| remaining.find(root).map(|offset| (offset, *root, slot)))
            .min_by(
                |(left_offset, left_root, _), (right_offset, right_root, _)| {
                    left_offset
                        .cmp(right_offset)
                        .then_with(|| right_root.len().cmp(&left_root.len()))
                },
            )
        else {
            output.push_str(remaining);
            break;
        };
        output.push_str(&remaining[..offset]);
        output.push_str(slot);
        remaining = &remaining[offset + root.len()..];
        if let Some(without_separator) = remaining.strip_prefix('/') {
            remaining = without_separator;
        }
        changed = true;
    }
    Ok(changed.then(|| OsString::from(output)))
}

fn relocated_environment_path(value: &OsStr, roots: &[PathBuf]) -> Result<Option<OsString>> {
    let path = PathBuf::from(value);
    let Some((root, relative)) = roots.iter().find_map(|root| {
        path.strip_prefix(root)
            .ok()
            .map(|relative| (root, relative))
    }) else {
        return Ok(None);
    };
    let mut relocated = execution_slot(&root.to_string_lossy())?;
    if !relative.as_os_str().is_empty() {
        relocated.push_str(&relative.to_string_lossy());
    }
    Ok(Some(OsString::from(relocated)))
}

fn option_values(args: &[OsString], option: &str) -> Vec<String> {
    let mut values = Vec::new();
    for (index, argument) in args.iter().enumerate() {
        let text = argument.to_string_lossy();
        let value = if text == option {
            args.get(index + 1).map(|value| value.to_string_lossy())
        } else {
            text.strip_prefix(&format!("{option}="))
                .map(std::borrow::Cow::Borrowed)
        };
        if let Some(value) = value {
            values.extend(value.split(',').map(ToOwned::to_owned));
        }
    }
    values
}

fn value_after<'a>(args: &'a [OsString], option: &str) -> Option<&'a OsStr> {
    args.windows(2)
        .find(|pair| pair[0] == option)
        .map(|pair| pair[1].as_os_str())
}

fn codegen_value(args: &[OsString], key: &str) -> Option<String> {
    args.windows(2)
        .filter(|pair| pair[0] == "-C")
        .filter_map(|pair| pair[1].to_str())
        .filter_map(|value| value.split_once('='))
        .find_map(|(name, value)| (name == key).then(|| value.to_owned()))
}

fn emit_values(args: &[OsString]) -> Vec<(String, Option<String>)> {
    let mut values = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        let text = arg.to_string_lossy();
        let raw = if text == "--emit" {
            args.get(index + 1).map(|value| value.to_string_lossy())
        } else {
            text.strip_prefix("--emit=").map(std::borrow::Cow::Borrowed)
        };
        if let Some(raw) = raw {
            values.extend(raw.split(',').map(|entry| {
                let (kind, path) = entry
                    .split_once('=')
                    .map_or((entry, None), |(kind, path)| (kind, Some(path.to_owned())));
                (kind.to_owned(), path)
            }));
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rustc_wrapper_shape() {
        let args = [
            "cargo-reapi",
            "/toolchain/bin/rustc",
            "--crate-name",
            "demo",
        ]
        .map(OsString::from);
        assert!(RustcInvocation::looks_like_wrapper(&args));
    }

    #[test]
    fn rejects_driver_shape() {
        let args = ["cargo-reapi", "reapi", "--", "test"].map(OsString::from);
        assert!(!RustcInvocation::looks_like_wrapper(&args));
    }

    #[test]
    fn does_not_classify_control_subcommands_as_compilers() {
        for command in ["contract", "prove"] {
            let args = vec![OsString::from("cargo-reapi"), OsString::from(command)];
            assert!(!RustcInvocation::looks_like_wrapper(&args));
        }
    }

    #[test]
    fn does_not_classify_canonical_cargo_commands_as_compilers() {
        for command in ["fmt", "check", "clippy", "test"] {
            let args = vec![OsString::from("cargo-reapi"), OsString::from(command)];
            assert!(!RustcInvocation::looks_like_wrapper(&args));
        }
    }

    #[test]
    fn keyed_compiles_receive_only_their_keyed_environment() {
        let keyed =
            CompileEnvironment::Keyed(std::collections::BTreeSet::from(["PATH".to_owned()]));
        let mut command = Command::new("rustc");
        apply_compiler_environment(
            &mut command,
            Path::new("/toolchain/bin/rustc"),
            Some(&keyed),
        )
        .expect("keyed environment");

        let passed = command
            .get_envs()
            .filter_map(|(name, value)| value.map(|_| name.to_string_lossy().into_owned()))
            .collect::<Vec<_>>();
        assert!(
            passed
                .iter()
                .all(|name| name == "PATH" || name.starts_with("CARGO_REAPI_"))
        );
    }

    #[test]
    fn relocates_cargo_binary_paths_into_the_target_slot() {
        let target = PathBuf::from("/workspace/target");
        let actual = OsStr::new("/workspace/target/debug/tool");
        let relocated = relocated_environment_value(actual, std::slice::from_ref(&target))
            .expect("relocate binary path");
        let expected = format!(
            "{}debug/tool",
            execution_slot(target.to_str().unwrap()).unwrap()
        );
        assert_eq!(relocated, Some(OsString::from(expected)));
    }

    #[test]
    fn relocates_paths_inside_arbitrary_environment_values_without_cascading_roots() {
        let package = PathBuf::from("/workspace/crate");
        let workspace = PathBuf::from("/workspace");
        let actual = OsStr::new("input=/workspace/crate/data;workspace=/workspace");
        let relocated = relocated_environment_value(actual, &[package.clone(), workspace.clone()])
            .expect("relocate composite value")
            .expect("value contains relocatable roots");
        let expected = format!(
            "input={}data;workspace={}",
            execution_slot(package.to_str().unwrap()).unwrap(),
            execution_slot(workspace.to_str().unwrap()).unwrap(),
        );
        assert_eq!(relocated, OsString::from(expected));
    }

    #[test]
    fn supports_nested_workspace_wrappers() {
        let args = [
            "cargo-reapi",
            "/tools/workspace-wrapper",
            "/toolchain/bin/rustc",
            "--crate-name",
            "demo",
        ]
        .map(OsString::from);
        assert!(RustcInvocation::looks_like_wrapper(&args));
    }

    #[test]
    fn compiler_observations_survive_pid_reuse_in_nested_namespaces() {
        let trace = tempfile::tempdir().expect("compiler trace");
        drop(create_observation_file(trace.path(), "control").expect("first observation"));
        drop(create_observation_file(trace.path(), "control").expect("second observation"));
        assert_eq!(
            fs::read_dir(trace.path()).expect("trace directory").count(),
            2
        );
    }

    #[test]
    fn parses_emit_paths_and_kinds() {
        let args = ["--emit=dep-info,metadata=/tmp/demo.rmeta,link"].map(OsString::from);
        assert_eq!(
            emit_values(&args),
            vec![
                ("dep-info".to_owned(), None),
                ("metadata".to_owned(), Some("/tmp/demo.rmeta".to_owned())),
                ("link".to_owned(), None),
            ]
        );
    }

    #[test]
    fn classifies_link_actions_from_emit_contract() {
        let invocation = RustcInvocation {
            compiler: PathBuf::from("rustc"),
            args: ["--emit=dep-info,metadata,link"]
                .map(OsString::from)
                .to_vec(),
            cwd: PathBuf::from("/workspace"),
        };
        assert!(invocation.is_link_action());

        let metadata_only = RustcInvocation {
            compiler: PathBuf::from("rustc"),
            args: ["--emit=dep-info,metadata"].map(OsString::from).to_vec(),
            cwd: PathBuf::from("/workspace"),
        };
        assert!(!metadata_only.is_link_action());
    }

    #[test]
    fn distinguishes_rust_archives_from_native_links() {
        let library = RustcInvocation {
            compiler: PathBuf::from("/toolchain/bin/rustc"),
            args: [
                "--crate-name",
                "demo",
                "--crate-type",
                "lib",
                "--emit=metadata,link",
            ]
            .map(OsString::from)
            .to_vec(),
            cwd: PathBuf::from("/workspace"),
        };
        assert!(library.is_link_action());
        assert!(!library.requires_native_linker());

        let mut proc_macro = library;
        let crate_type = proc_macro
            .args
            .iter()
            .position(|value| value == "lib")
            .expect("crate type");
        proc_macro.args[crate_type] = OsString::from("proc-macro");
        assert!(proc_macro.requires_native_linker());
    }

    #[test]
    fn executable_and_test_metadata_emit_declare_the_real_rmeta() {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let source = fixture.path().join("demo.rs");
        std::fs::write(&source, "fn main() {}\n").expect("fixture source");
        let output_directory = fixture.path().join("out");
        std::fs::create_dir(&output_directory).expect("output directory");
        let mut invocation = RustcInvocation {
            compiler: PathBuf::from("rustc"),
            args: vec![
                OsString::from("--crate-name"),
                OsString::from("demo"),
                OsString::from("--crate-type"),
                OsString::from("bin"),
                OsString::from("--emit=dep-info,metadata"),
                OsString::from("-C"),
                OsString::from("extra-filename=-abc"),
                OsString::from("--out-dir"),
                output_directory.clone().into_os_string(),
                source.into_os_string(),
            ],
            cwd: PathBuf::from("/workspace"),
        };
        let expected = vec![
            output_directory.join("demo-abc.d"),
            output_directory.join("libdemo-abc.rmeta"),
        ];
        assert_eq!(invocation.output_files().expect("bin outputs"), expected);
        assert_eq!(invocation.execute().expect("execute metadata-only bin"), 0);
        assert!(expected.iter().all(|path| path.is_file()));

        invocation.args.push(OsString::from("--test"));
        assert_eq!(invocation.output_files().expect("test outputs"), expected);
        assert_eq!(invocation.execute().expect("execute metadata-only test"), 0);
        assert!(expected.iter().all(|path| path.is_file()));
    }
}
