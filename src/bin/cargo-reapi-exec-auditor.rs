use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long, value_enum)]
    format: EventFormat,
    #[arg(long)]
    evidence_root: PathBuf,
    #[arg(long)]
    events: PathBuf,
    #[arg(long)]
    observer_stderr: PathBuf,
    #[arg(long)]
    selection_config: PathBuf,
    #[arg(long)]
    normalized_events: PathBuf,
    #[arg(long)]
    report: PathBuf,
    #[arg(long)]
    wrapper_trace: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EventFormat {
    MacosEslogger,
    LinuxStrace,
}

#[derive(Debug, Deserialize)]
struct SelectionConfig {
    schema_version: u32,
    observer_kind: String,
    observer_version: String,
    observer_command: Vec<String>,
    selected_executables: Vec<SelectedExecutable>,
    expected: String,
    attribution_root: Option<PathBuf>,
    #[serde(default)]
    expected_crates: BTreeSet<String>,
    #[serde(default)]
    coalescing_roots: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct SelectedExecutable {
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Debug, Serialize)]
struct NormalizedExec {
    executable: String,
    cwd: Option<String>,
    arguments: Vec<String>,
    crate_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct EvidenceRef {
    role: String,
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Serialize)]
struct ExecAuditReport {
    schema_version: u32,
    observer_kind: String,
    observer_version: String,
    observer_command: Vec<String>,
    expected: String,
    total_event_count: usize,
    selected_event_count: usize,
    invalid_event_count: usize,
    os_derived_crates: BTreeSet<String>,
    wrapper_derived_crates: BTreeSet<String>,
    attribution_sets_equal: bool,
    coalescing_root_event_counts: BTreeMap<String, usize>,
    evidence_refs: Vec<EvidenceRef>,
    violations: Vec<String>,
    passed: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cargo-reapi-exec-auditor: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let root = cli.evidence_root.canonicalize()?;
    let events = existing_under(&root, &cli.events)?;
    let stderr = existing_under(&root, &cli.observer_stderr)?;
    let selection_path = existing_under(&root, &cli.selection_config)?;
    let selection: SelectionConfig = serde_json::from_slice(&fs::read(&selection_path)?)?;
    if selection.schema_version != 1 {
        bail!("unsupported selection schema {}", selection.schema_version);
    }
    verify_selected_executables(&selection)?;
    let (normalized, total_event_count, invalid_event_count) = match cli.format {
        EventFormat::MacosEslogger => parse_eslogger(&events)?,
        EventFormat::LinuxStrace => parse_strace(&events)?,
    };
    let selected = normalized
        .iter()
        .filter(|event| executable_selected(&event.executable, &selection))
        .cloned()
        .collect::<Vec<_>>();
    write_jsonl(&cli.normalized_events, &selected)?;

    let mut violations = Vec::new();
    if invalid_event_count != 0 {
        violations.push(format!(
            "observer stream contains {invalid_event_count} invalid events"
        ));
    }
    match selection.expected.as_str() {
        "zero" if !selected.is_empty() => violations.push(format!(
            "OS observer recorded {} selected compiler/linker events",
            selected.len()
        )),
        "nonzero" if selected.is_empty() => {
            violations.push("OS observer recorded no selected compiler/linker event".to_owned());
        }
        "zero" | "nonzero" | "attribution" | "coalescing" => {}
        value => violations.push(format!("unknown selection expectation {value}")),
    }

    let os_derived_crates = if selection.expected == "attribution" {
        derive_os_crates(
            &selected,
            selection.attribution_root.as_deref(),
            &mut violations,
        )
    } else {
        BTreeSet::new()
    };
    let wrapper_derived_crates = if selection.expected == "attribution" {
        let trace = cli
            .wrapper_trace
            .as_deref()
            .context("attribution requires --wrapper-trace")?;
        derive_wrapper_crates(trace, selection.attribution_root.as_deref())?
    } else {
        BTreeSet::new()
    };
    let attribution_sets_equal = os_derived_crates == wrapper_derived_crates;
    if selection.expected == "attribution" {
        if os_derived_crates != selection.expected_crates {
            violations.push(format!(
                "OS-derived rebuild set is {os_derived_crates:?}; expected {:?}",
                selection.expected_crates
            ));
        }
        if !attribution_sets_equal {
            violations.push(format!(
                "OS-derived rebuild set {os_derived_crates:?} disagrees with wrapper set {wrapper_derived_crates:?}"
            ));
        }
    }
    let coalescing_root_event_counts = if selection.expected == "coalescing" {
        let counts = selection
            .coalescing_roots
            .iter()
            .map(|root| {
                (
                    root.display().to_string(),
                    selected
                        .iter()
                        .filter(|event| event_belongs_to_root(event, root))
                        .count(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if counts.len() != 2
            || counts.values().filter(|count| **count > 0).count() != 1
            || counts.values().filter(|count| **count == 0).count() != 1
        {
            violations.push(format!(
                "coalescing OS event distribution is {counts:?}; expected one producer and one waiter"
            ));
        }
        counts
    } else {
        BTreeMap::new()
    };

    let normalized_path = existing_under(&root, &cli.normalized_events)?;
    let mut evidence_refs = vec![
        evidence_ref(&root, "raw_os_events", &events)?,
        evidence_ref(&root, "observer_stderr", &stderr)?,
        evidence_ref(&root, "selection_config", &selection_path)?,
        evidence_ref(&root, "normalized_os_events", &normalized_path)?,
    ];
    if let Some(trace) = &cli.wrapper_trace {
        evidence_refs.push(evidence_ref(
            &root,
            "wrapper_attribution_crosscheck",
            &existing_under(&root, trace)?,
        )?);
    }
    let report = ExecAuditReport {
        schema_version: 2,
        observer_kind: selection.observer_kind,
        observer_version: selection.observer_version,
        observer_command: selection.observer_command,
        expected: selection.expected,
        total_event_count,
        selected_event_count: selected.len(),
        invalid_event_count,
        os_derived_crates,
        wrapper_derived_crates,
        attribution_sets_equal,
        coalescing_root_event_counts,
        evidence_refs,
        passed: violations.is_empty(),
        violations,
    };
    write_json(&cli.report, &report)?;
    if !report.passed {
        bail!("exec evidence failed; report: {}", cli.report.display());
    }
    Ok(())
}

fn verify_selected_executables(selection: &SelectionConfig) -> Result<()> {
    if selection.selected_executables.is_empty() {
        bail!("selection config has no selected executables");
    }
    for executable in &selection.selected_executables {
        let actual = sha256_file(&executable.path).with_context(|| {
            format!("hashing selected executable {}", executable.path.display())
        })?;
        if actual != executable.sha256 {
            bail!(
                "selected executable {} changed: expected {}, got {actual}",
                executable.path.display(),
                executable.sha256
            );
        }
    }
    Ok(())
}

fn executable_selected(executable: &str, selection: &SelectionConfig) -> bool {
    selection
        .selected_executables
        .iter()
        .any(|selected| selected.path == Path::new(executable))
}

fn parse_eslogger(path: &Path) -> Result<(Vec<NormalizedExec>, usize, usize)> {
    let input = fs::read_to_string(path)?;
    let mut events = Vec::new();
    let mut total = 0;
    let mut invalid = 0;
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        total += 1;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            invalid += 1;
            continue;
        };
        let Some(executable) = value
            .pointer("/event/exec/target/executable/path")
            .and_then(serde_json::Value::as_str)
        else {
            invalid += 1;
            continue;
        };
        let arguments = value
            .pointer("/event/exec/args")
            .and_then(serde_json::Value::as_array)
            .map(|arguments| {
                arguments
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let cwd = value
            .pointer("/event/exec/cwd/path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        events.push(NormalizedExec {
            executable: executable.to_owned(),
            cwd,
            crate_name: crate_name(&arguments),
            arguments,
        });
    }
    Ok((events, total, invalid))
}

fn parse_strace(path: &Path) -> Result<(Vec<NormalizedExec>, usize, usize)> {
    let input = fs::read_to_string(path)?;
    let mut events = Vec::new();
    let mut total = 0;
    let mut invalid = 0;
    for line in input.lines().filter(|line| line.contains("execve(")) {
        total += 1;
        match parse_strace_execve(line) {
            Some(event) => events.push(event),
            None => invalid += 1,
        }
    }
    Ok((events, total, invalid))
}

fn parse_strace_execve(line: &str) -> Option<NormalizedExec> {
    let body = line.split_once("execve(")?.1;
    let path_start = body.find('"')?;
    let after_start = &body[path_start..];
    let path_value: String =
        serde_json::from_str(after_start.get(..json_string_end(after_start)?)?).ok()?;
    let args_start = body.find(", [")? + 2;
    let args_body = &body[args_start..];
    let args_end = args_body.find("], ")? + 1;
    let arguments: Vec<String> = serde_json::from_str(&args_body[..args_end]).ok()?;
    Some(NormalizedExec {
        executable: path_value,
        cwd: None,
        crate_name: crate_name(&arguments),
        arguments,
    })
}

fn json_string_end(value: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate().skip(1) {
        if byte == b'"' && !escaped {
            return Some(index + 1);
        }
        escaped = byte == b'\\' && !escaped;
        if byte != b'\\' {
            escaped = false;
        }
    }
    None
}

fn crate_name(arguments: &[String]) -> Option<String> {
    arguments
        .windows(2)
        .find(|pair| pair[0] == "--crate-name")
        .map(|pair| pair[1].clone())
        .filter(|name| name != "___")
}

fn derive_os_crates(
    events: &[NormalizedExec],
    attribution_root: Option<&Path>,
    violations: &mut Vec<String>,
) -> BTreeSet<String> {
    let Some(root) = attribution_root else {
        violations.push("attribution selection has no worktree root".to_owned());
        return BTreeSet::new();
    };
    events
        .iter()
        .filter(|event| event_belongs_to_root(event, root))
        .filter_map(|event| event.crate_name.clone())
        .collect()
}

fn event_belongs_to_root(event: &NormalizedExec, root: &Path) -> bool {
    event
        .cwd
        .as_deref()
        .is_some_and(|cwd| Path::new(cwd).starts_with(root))
        || event.arguments.iter().any(|argument| {
            Path::new(argument).starts_with(root) || argument.contains(&root.display().to_string())
        })
}

fn derive_wrapper_crates(trace: &Path, root: Option<&Path>) -> Result<BTreeSet<String>> {
    let root = root.context("attribution selection has no worktree root")?;
    let mut crates = BTreeSet::new();
    if trace.is_file() {
        for line in fs::read_to_string(trace)?.lines() {
            let value: serde_json::Value = serde_json::from_str(line)?;
            if value["kind"] == "compile"
                && value["cwd"]
                    .as_str()
                    .is_some_and(|cwd| Path::new(cwd).starts_with(root))
                && let Some(name) = value["crate_name"].as_str()
            {
                crates.insert(name.to_owned());
            }
        }
        return Ok(crates);
    }
    for entry in fs::read_dir(trace)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let input = fs::read_to_string(entry.path())?;
        let kind = input.lines().find_map(|line| line.strip_prefix("kind="));
        let cwd = input.lines().find_map(|line| line.strip_prefix("cwd="));
        let name = input
            .lines()
            .find_map(|line| line.strip_prefix("crate_name="));
        if kind == Some("compile")
            && cwd.is_some_and(|cwd| Path::new(cwd).starts_with(root))
            && let Some(name) = name
        {
            crates.insert(name.to_owned());
        }
    }
    Ok(crates)
}

fn existing_under(root: &Path, path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing evidence {}", path.display()))?;
    if !canonical.starts_with(root) || fs::symlink_metadata(&canonical)?.file_type().is_symlink() {
        bail!("evidence is outside the sealed root: {}", path.display());
    }
    Ok(canonical)
}

fn evidence_ref(root: &Path, role: &str, path: &Path) -> Result<EvidenceRef> {
    Ok(EvidenceRef {
        role: role.to_owned(),
        path: path.strip_prefix(root)?.to_path_buf(),
        sha256: sha256_file(path)?,
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(fs::read(path)?)))
}

fn write_jsonl(path: &Path, events: &[NormalizedExec]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = String::new();
    for event in events {
        output.push_str(&serde_json::to_string(event)?);
        output.push('\n');
    }
    fs::write(path, output)?;
    Ok(())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{crate_name, parse_eslogger, parse_strace_execve};

    #[test]
    fn parses_strace_exec_arguments() {
        let event = parse_strace_execve(
            r#"123 execve("/usr/bin/rustc", ["rustc", "--crate-name", "leaf"], 0x123 /* 4 vars */) = 0"#,
        )
        .expect("strace event");
        assert_eq!(event.executable, "/usr/bin/rustc");
        assert_eq!(event.crate_name.as_deref(), Some("leaf"));
    }

    #[test]
    fn parses_eslogger_arguments_and_cwd() {
        let root = tempfile::tempdir().expect("root");
        let events = root.path().join("events.jsonl");
        std::fs::write(
            &events,
            r#"{"event":{"exec":{"target":{"executable":{"path":"/rustc"}},"cwd":{"path":"/work"},"args":["rustc","--crate-name","mid"]}}}
"#,
        )
        .expect("events");
        let (parsed, total, invalid) = parse_eslogger(&events).expect("parse");
        assert_eq!((total, invalid), (1, 0));
        assert_eq!(parsed[0].crate_name.as_deref(), Some("mid"));
        assert_eq!(parsed[0].cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn crate_name_ignores_control_probes() {
        assert_eq!(
            crate_name(&["--crate-name".to_owned(), "___".to_owned()]),
            None
        );
    }
}
