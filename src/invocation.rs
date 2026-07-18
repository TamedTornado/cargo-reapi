use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub struct RustcInvocation {
    pub compiler: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

impl RustcInvocation {
    pub fn looks_like_wrapper(args: &[OsString]) -> bool {
        args.get(1)
            .is_some_and(|value| value != "reapi" && !value.to_string_lossy().starts_with('-'))
    }

    pub fn parse(args: Vec<OsString>) -> Result<Self> {
        let compiler = args.get(1).context("missing rustc path")?;
        Ok(Self {
            compiler: PathBuf::from(compiler),
            args: args.into_iter().skip(2).collect(),
            cwd: std::env::current_dir().context("reading wrapper working directory")?,
        })
    }

    pub fn execute(&self) -> Result<i32> {
        let status = Command::new(&self.compiler)
            .args(&self.args)
            .status()
            .with_context(|| format!("executing {}", self.compiler.display()))?;
        Ok(status.code().unwrap_or(1))
    }

    pub fn crate_name(&self) -> Option<&OsStr> {
        value_after(&self.args, "--crate-name")
    }

    pub fn out_dir(&self) -> Option<PathBuf> {
        value_after(&self.args, "--out-dir").map(PathBuf::from)
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
        let link_name = String::from_utf8(printed.stdout)
            .context("rustc returned a non-UTF-8 output filename")?
            .lines()
            .next()
            .context("rustc returned no output filename")?
            .to_owned();
        let mut outputs = Vec::new();
        for (kind, explicit_path) in emits {
            if let Some(path) = explicit_path {
                outputs.push(PathBuf::from(path));
                continue;
            }
            match kind.as_str() {
                "link" => outputs.push(out_dir.join(&link_name)),
                "metadata" => {
                    outputs.push(out_dir.join(Path::new(&link_name).with_extension("rmeta")));
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
}
