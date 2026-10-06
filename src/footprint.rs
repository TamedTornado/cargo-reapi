//! The physical disk footprint of a set of files.
//!
//! Copy-on-write file systems let files share physical blocks: a reflinked gate
//! snapshot occupies almost no new disk although every file keeps its full
//! length. Summing lengths counts each snapshot again in full, so a collector
//! working from that sum evicts reusable entries to free space that was never
//! used. The footprint instead counts each physical byte once, from the file
//! system's extent map, and knows which bytes deleting a set of files frees.
//!
//! When a file has no exact extent map (the platform or file system cannot
//! report one, or an extent's placement is not an exact block range), its
//! allocated size counts as unshared. That can over-count shared data but never
//! under-counts it, so a budget is never exceeded silently.

use std::fs;
use std::io;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};

/// Identifies a set of files that are removed together, such as one action or
/// one gate snapshot. Bytes held by an owner that is never released stay in
/// the footprint.
pub type OwnerId = usize;

/// Collects every file's physical placement before the footprint is built.
#[derive(Default)]
pub struct FootprintBuilder {
    extents: Vec<OwnedExtent>,
    unshared: Vec<u64>,
}

struct OwnedExtent {
    device: u64,
    start: u64,
    end: u64,
    owner: OwnerId,
}

enum Placement {
    Exact(Vec<(u64, u64)>),
    Unshared(u64),
}

impl FootprintBuilder {
    /// Records `path` as held by `owner`. A file removed concurrently is
    /// skipped: it no longer occupies the disk.
    pub fn add_file(&mut self, owner: OwnerId, path: &Path) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        if !metadata.is_file() {
            return Ok(());
        }
        if self.unshared.len() <= owner {
            self.unshared.resize(owner + 1, 0);
        }

        let placement = match placement(path, &metadata) {
            Ok(placement) => placement,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading the extent map of {}", path.display()));
            }
        };
        match placement {
            Placement::Exact(ranges) => {
                let device = device(&metadata);
                self.extents
                    .extend(ranges.into_iter().map(|(start, end)| OwnedExtent {
                        device,
                        start,
                        end,
                        owner,
                    }));
            }
            Placement::Unshared(bytes) => {
                self.unshared[owner] = self.unshared[owner].saturating_add(bytes);
            }
        }
        Ok(())
    }

    /// Splits every device's extents at their boundaries, so each resulting
    /// segment is referenced by a fixed set of owners.
    pub fn build(self) -> Footprint {
        let mut boundaries = self
            .extents
            .iter()
            .flat_map(|extent| [(extent.device, extent.start), (extent.device, extent.end)])
            .collect::<Vec<_>>();
        boundaries.sort_unstable();
        boundaries.dedup();

        let segment_lengths = boundaries
            .windows(2)
            .map(|pair| {
                if pair[0].0 == pair[1].0 {
                    pair[1].1 - pair[0].1
                } else {
                    0
                }
            })
            .collect::<Vec<_>>();
        let mut references = vec![0_u32; segment_lengths.len()];
        let mut owner_segments = vec![Vec::new(); self.unshared.len()];

        for extent in &self.extents {
            let first = boundaries
                .binary_search(&(extent.device, extent.start))
                .expect("every extent start is a boundary");
            let last = boundaries
                .binary_search(&(extent.device, extent.end))
                .expect("every extent end is a boundary");
            for count in &mut references[first..last] {
                *count += 1;
            }
            owner_segments[extent.owner].push(first..last);
        }

        let shared_bytes = segment_lengths
            .iter()
            .zip(&references)
            .filter(|(_, count)| **count > 0)
            .map(|(length, _)| *length)
            .sum::<u64>();
        let unshared_bytes = self.unshared.iter().sum::<u64>();
        let released = vec![false; self.unshared.len()];

        Footprint {
            segment_lengths,
            references,
            owner_segments,
            unshared: self.unshared,
            released,
            total_bytes: shared_bytes.saturating_add(unshared_bytes),
        }
    }
}

/// The bytes a set of owners occupies, counting each physical byte once.
pub struct Footprint {
    segment_lengths: Vec<u64>,
    references: Vec<u32>,
    owner_segments: Vec<Vec<Range<usize>>>,
    unshared: Vec<u64>,
    released: Vec<bool>,
    total_bytes: u64,
}

impl Footprint {
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Removes one owner's files from the footprint and returns the bytes no
    /// remaining owner references: what deleting those files frees. Releasing
    /// an owner twice, or one with no files, frees nothing.
    pub fn release(&mut self, owner: OwnerId) -> u64 {
        if self.released.get(owner).copied().unwrap_or(true) {
            return 0;
        }
        self.released[owner] = true;

        let mut freed = self.unshared[owner];
        for range in &self.owner_segments[owner] {
            for index in range.clone() {
                self.references[index] -= 1;
                if self.references[index] == 0 {
                    freed += self.segment_lengths[index];
                }
            }
        }
        self.total_bytes = self.total_bytes.saturating_sub(freed);
        freed
    }
}

fn placement(path: &Path, metadata: &fs::Metadata) -> io::Result<Placement> {
    #[cfg(target_os = "linux")]
    if let Some(ranges) = linux::exact_placement(path)? {
        return Ok(Placement::Exact(ranges));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
    Ok(Placement::Unshared(allocated_bytes(metadata)))
}

#[cfg(unix)]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    // st_blocks counts 512-byte units on every Unix, whatever the block size.
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

#[cfg(unix)]
fn device(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    metadata.dev()
}

#[cfg(not(unix))]
fn device(_metadata: &fs::Metadata) -> u64 {
    0
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    // Linux UAPI <linux/fiemap.h> and <linux/fs.h>. musl declares the ioctl
    // request as a signed int; the request bits are the same.
    #[cfg(not(target_env = "musl"))]
    const FS_IOC_FIEMAP: libc::Ioctl = 0xC020_660B;
    #[cfg(target_env = "musl")]
    const FS_IOC_FIEMAP: libc::Ioctl = libc::Ioctl::from_ne_bytes(0xC020_660B_u32.to_ne_bytes());
    const FIEMAP_FLAG_SYNC: u32 = 0x1;
    const FIEMAP_EXTENT_LAST: u32 = 0x1;
    /// Placements that are not an exact block range owned by this extent:
    /// unknown, delayed, encoded, encrypted, unaligned, inline or tail data.
    const FIEMAP_EXTENT_INEXACT: u32 = 0x2 | 0x4 | 0x8 | 0x80 | 0x100 | 0x200 | 0x400;
    /// Extents requested per call; a fixed protocol buffer, not a policy.
    const EXTENTS_PER_CALL: usize = 256;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }

    #[repr(C)]
    struct Request {
        start: u64,
        length: u64,
        flags: u32,
        mapped_extents: u32,
        extent_count: u32,
        reserved: u32,
        extents: [Extent; EXTENTS_PER_CALL],
    }

    /// Returns the file's physical byte ranges, or `None` when the file system
    /// has no extent map or any extent's placement is not exact.
    pub(super) fn exact_placement(path: &Path) -> io::Result<Option<Vec<(u64, u64)>>> {
        let file = File::open(path)?;
        match physical_extents(&file) {
            // A file system without FIEMAP still reports an allocated size.
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EINVAL)
                ) =>
            {
                Ok(None)
            }
            result => result,
        }
    }

    /// `FIEMAP_FLAG_SYNC` flushes dirty data first, so no delayed allocation
    /// hides behind an unmapped range.
    fn physical_extents(file: &File) -> io::Result<Option<Vec<(u64, u64)>>> {
        let mut ranges = Vec::new();
        let mut logical = 0_u64;
        loop {
            let mut request = Request {
                start: logical,
                length: u64::MAX,
                flags: FIEMAP_FLAG_SYNC,
                mapped_extents: 0,
                extent_count: u32::try_from(EXTENTS_PER_CALL).expect("extent batch fits in u32"),
                reserved: 0,
                extents: [Extent::default(); EXTENTS_PER_CALL],
            };
            // SAFETY: `request` is an initialized `struct fiemap` followed by
            // room for `extent_count` extents. The kernel writes no more than
            // that, and only for the duration of this call.
            let status = unsafe {
                libc::ioctl(
                    file.as_raw_fd(),
                    FS_IOC_FIEMAP,
                    std::ptr::from_mut(&mut request),
                )
            };
            if status < 0 {
                return Err(io::Error::last_os_error());
            }

            let mapped = usize::try_from(request.mapped_extents).expect("u32 fits in usize");
            let Some(last) = request.extents[..mapped].last().copied() else {
                return Ok(Some(ranges));
            };
            for extent in &request.extents[..mapped] {
                if extent.flags & FIEMAP_EXTENT_INEXACT != 0 {
                    return Ok(None);
                }
                ranges.push((
                    extent.physical,
                    extent.physical.saturating_add(extent.length),
                ));
                if extent.flags & FIEMAP_EXTENT_LAST != 0 {
                    return Ok(Some(ranges));
                }
            }
            logical = last.logical.saturating_add(last.length);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(device: u64, ranges: &[(u64, u64)], owner: OwnerId) -> Vec<OwnedExtent> {
        ranges
            .iter()
            .map(|&(start, end)| OwnedExtent {
                device,
                start,
                end,
                owner,
            })
            .collect()
    }

    fn footprint(extents: Vec<OwnedExtent>, unshared: Vec<u64>) -> Footprint {
        FootprintBuilder { extents, unshared }.build()
    }

    #[test]
    fn shared_ranges_count_once_and_free_only_with_their_last_owner() {
        let mut extents = exact(1, &[(0, 100)], 0);
        extents.extend(exact(1, &[(0, 100)], 1));
        extents.extend(exact(1, &[(100, 150)], 1));
        let mut footprint = footprint(extents, vec![0, 0]);

        assert_eq!(footprint.total_bytes(), 150);
        assert_eq!(footprint.release(0), 0);
        assert_eq!(footprint.release(1), 150);
        assert_eq!(footprint.total_bytes(), 0);
    }

    #[test]
    fn partial_overlaps_split_into_independently_referenced_segments() {
        let mut extents = exact(1, &[(0, 100)], 0);
        extents.extend(exact(1, &[(60, 160)], 1));
        let mut footprint = footprint(extents, vec![0, 0]);

        assert_eq!(footprint.total_bytes(), 160);
        assert_eq!(footprint.release(1), 60);
        assert_eq!(footprint.release(0), 100);
    }

    #[test]
    fn equal_offsets_on_different_devices_are_different_bytes() {
        let mut extents = exact(1, &[(0, 100)], 0);
        extents.extend(exact(2, &[(0, 100)], 1));
        let mut footprint = footprint(extents, vec![0, 0]);

        assert_eq!(footprint.total_bytes(), 200);
        assert_eq!(footprint.release(0), 100);
    }

    #[test]
    fn unshared_bytes_are_freed_with_their_owner_and_release_is_idempotent() {
        let mut footprint = footprint(Vec::new(), vec![40, 2]);

        assert_eq!(footprint.total_bytes(), 42);
        assert_eq!(footprint.release(0), 40);
        assert_eq!(footprint.release(0), 0);
        assert_eq!(footprint.release(7), 0);
        assert_eq!(footprint.total_bytes(), 2);
    }
}
