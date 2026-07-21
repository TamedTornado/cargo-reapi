use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const QUERY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
struct QueryResponse {
    schema_version: u32,
    key: String,
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

pub fn cargo_metadata_output(
    workspace: &Path,
    cache: &Path,
    arguments: &[&str],
) -> Result<std::process::Output> {
    let executable = env::current_exe().context("locating cargo-reapi query shim")?;
    let compiler = env::var_os("RUSTC")
        .map(PathBuf::from)
        .or_else(|| resolve_executable("rustc"))
        .context("resolving rustc for Cargo metadata")?;
    let mut command = Command::new("cargo");
    command
        .args(["metadata", "--format-version", "1"])
        .args(arguments)
        .current_dir(workspace)
        .env("RUSTC", executable)
        .env_remove("RUSTC_WRAPPER")
        .env("CARGO_REAPI_RUSTC_QUERY_SHIM", "1")
        .env("CARGO_REAPI_RUSTC_QUERY_COMPILER", compiler)
        .env("CARGO_REAPI_RUSTC_QUERY_CACHE", cache);
    command
        .output()
        .context("running compiler-free Cargo metadata")
}

pub fn run_shim(arguments: &[OsString]) -> Result<i32> {
    let compiler = PathBuf::from(
        env::var_os("CARGO_REAPI_RUSTC_QUERY_COMPILER")
            .context("query shim is missing its compiler")?,
    );
    let cache = PathBuf::from(
        env::var_os("CARGO_REAPI_RUSTC_QUERY_CACHE").context("query shim is missing its cache")?,
    );
    let mut stdin = Vec::new();
    std::io::stdin()
        .read_to_end(&mut stdin)
        .context("reading rustc query stdin")?;
    let response = cached_query(&compiler, &arguments[1..], &stdin, &cache)?;
    std::io::stdout().write_all(&response.stdout)?;
    std::io::stderr().write_all(&response.stderr)?;
    Ok(response.exit_code)
}

fn cached_query(
    compiler: &Path,
    arguments: &[OsString],
    stdin: &[u8],
    cache: &Path,
) -> Result<QueryResponse> {
    let key = query_key(compiler, arguments, stdin)?;
    let root = cache.join("rustc-query-cache-v1");
    fs::create_dir_all(root.join("objects"))?;
    fs::create_dir_all(root.join("locks"))?;
    let object = root.join("objects").join(format!("{key}.json"));
    if let Some(response) = load_response(&object, &key)? {
        return Ok(response);
    }

    let lock_path = root.join("locks").join(format!("{key}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    lock.lock_exclusive()?;
    if let Some(response) = load_response(&object, &key)? {
        FileExt::unlock(&lock)?;
        return Ok(response);
    }

    let mut command = Command::new(compiler);
    command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("executing rustc query through {}", compiler.display()))?;
    child
        .stdin
        .take()
        .context("rustc query stdin is unavailable")?
        .write_all(stdin)?;
    let output = child.wait_with_output()?;
    let response = QueryResponse {
        schema_version: QUERY_SCHEMA_VERSION,
        key: key.clone(),
        exit_code: output.status.code().unwrap_or(1),
        stdout: output.stdout,
        stderr: output.stderr,
    };
    let temporary = object.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, serde_json::to_vec(&response)?)?;
    fs::rename(&temporary, &object)?;
    FileExt::unlock(&lock)?;
    Ok(response)
}

fn load_response(path: &Path, expected_key: &str) -> Result<Option<QueryResponse>> {
    let Ok(bytes) = fs::read(path) else {
        return Ok(None);
    };
    let response: QueryResponse = serde_json::from_slice(&bytes)?;
    if response.schema_version != QUERY_SCHEMA_VERSION || response.key != expected_key {
        return Ok(None);
    }
    Ok(Some(response))
}

fn query_key(compiler: &Path, arguments: &[OsString], stdin: &[u8]) -> Result<String> {
    let compiler = fs::canonicalize(compiler).unwrap_or_else(|_| compiler.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(b"cargo-reapi-rustc-query-v1\0");
    hash_field(&mut hasher, env::consts::OS.as_bytes());
    hash_field(&mut hasher, env::consts::ARCH.as_bytes());
    hash_field(&mut hasher, compiler.to_string_lossy().as_bytes());
    hash_field(
        &mut hasher,
        &fs::read(&compiler)
            .with_context(|| format!("hashing query compiler {}", compiler.display()))?,
    );
    if let Some(real) = env::var_os("CARGO_REAPI_REAL_RUSTC").map(PathBuf::from)
        && real.is_file()
    {
        hash_field(&mut hasher, &fs::read(real)?);
    }
    for argument in arguments {
        hash_field(&mut hasher, argument.to_string_lossy().as_bytes());
    }
    hash_field(&mut hasher, stdin);
    for name in [
        "CARGO_ENCODED_RUSTFLAGS",
        "MACOSX_DEPLOYMENT_TARGET",
        "RUSTFLAGS",
        "SDKROOT",
    ] {
        if let Some(value) = env::var_os(name) {
            hash_field(&mut hasher, name.as_bytes());
            hash_field(&mut hasher, value.to_string_lossy().as_bytes());
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(value.len().to_le_bytes());
    hasher.update(value);
}

fn resolve_executable(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|root| root.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn identical_query_executes_the_compiler_once() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        let compiler = fixture.path().join("compiler");
        let counter = fixture.path().join("counter");
        fs::write(
            &compiler,
            format!(
                "#!/bin/sh\nprintf x >>'{}'\nprintf answer\n",
                counter.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
        let arguments = [OsString::from("-vV")];
        let first = cached_query(&compiler, &arguments, b"", fixture.path()).unwrap();
        let second = cached_query(&compiler, &arguments, b"", fixture.path()).unwrap();
        assert_eq!(first.stdout, b"answer");
        assert_eq!(second.stdout, b"answer");
        assert_eq!(fs::read(&counter).unwrap(), b"x");
    }
}
