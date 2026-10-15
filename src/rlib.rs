//! What an rlib holds besides the crate's own compiled code.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};

/// Whether the rlib at `path` holds only rustc's metadata and the object
/// files rustc compiled from crate `crate_name` itself.
///
/// rustc's metadata is `lib.rmeta`, and from Rust 1.97 also `lib.rmeta-link`,
/// a small object rustc writes that lists the crate's own object files.
///
/// rustc names its own objects `<crate>-<extra filename>.<codegen unit>.rcgu.o`
/// (`<crate>.<codegen unit>.rcgu.o` without an extra filename). Any other
/// member was copied in from a static library the rlib bundles. The archive
/// reader skips the symbol table and long-name table; a member it cannot
/// read makes the answer `false`.
pub fn holds_only_own_members(path: &Path, crate_name: &str) -> Result<bool> {
    let file = File::open(path).with_context(|| format!("opening rlib {}", path.display()))?;
    let mut archive = ar::Archive::new(file);

    while let Some(entry) = archive.next_entry() {
        let Ok(entry) = entry else {
            return Ok(false);
        };
        if !is_own_member(entry.header().identifier(), crate_name) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn is_own_member(identifier: &[u8], crate_name: &str) -> bool {
    let Ok(name) = std::str::from_utf8(identifier) else {
        return false;
    };
    if name == "lib.rmeta" || name == "lib.rmeta-link" {
        return true;
    }

    let Some(rest) = name.strip_prefix(crate_name) else {
        return false;
    };
    (rest.starts_with('-') || rest.starts_with('.')) && rest.ends_with(".rcgu.o")
}

#[cfg(test)]
mod tests {
    use super::is_own_member;

    #[test]
    fn only_the_metadata_and_the_crates_own_objects_are_its_own() {
        assert!(is_own_member(b"lib.rmeta", "my_crate"));
        assert!(is_own_member(b"lib.rmeta-link", "my_crate"));
        assert!(is_own_member(
            b"my_crate-abc123.my_crate.9d2e7ca3e9e4b14f-cgu.0.rcgu.o",
            "my_crate"
        ));
        assert!(is_own_member(
            b"my_crate.6sw3ggbqse0wwxi2dw6kwkq9j.1x2c51b.rcgu.o",
            "my_crate"
        ));

        assert!(!is_own_member(b"answer.o", "my_crate"));
        assert!(!is_own_member(
            b"other-abc123.other.9d2e7ca3e9e4b14f-cgu.0.rcgu.o",
            "my_crate"
        ));
        assert!(!is_own_member(
            b"my_crate_sys-abc123.my_crate_sys.9d2e-cgu.0.rcgu.o",
            "my_crate"
        ));
    }
}
