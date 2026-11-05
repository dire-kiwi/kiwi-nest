//! The disk space a subvolume's files take, from their extents (`FS_IOC_FIEMAP`), as `btrfs filesystem du` counts
//! it, without `btrfs-progs`.
//!
//! Each regular file's extents are listed; those the kernel marks shared (with a snapshot, or a reflinked copy) are
//! kept as ranges of the filesystem's address space, so a caller can count data that several subvolumes share once
//! ([`union_bytes`]). The walk opens every directory and file without following symlinks, stays within the
//! subvolume (a nested subvolume or a mount is another device), counts each inode once (hard links), and opens files
//! non-blocking, after checking they're regular, so nothing but reading metadata happens. Lengths are the data's
//! (before compression), and metadata (directories, inline extents' overhead) isn't counted.
//!
//! On a filesystem without `FIEMAP` (such as tmpfs, for the fake backend in tests) a file counts its allocated
//! blocks, as exclusive.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::Path;

use crate::{Error, Result, fstat, lstat_at, open_dir, openat};

/// What a tree of files takes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeUsage {
    /// Every extent of every file.
    pub bytes: u64,
    /// The extents the kernel doesn't mark shared.
    pub exclusive_bytes: u64,
    /// The shared extents, as `(start, length)` in the filesystem's address space, for [`union_bytes`].
    pub shared: Vec<(u64, u64)>,
    /// Files and directories that couldn't be read (permissions, or removed during the walk), so weren't counted.
    pub skipped: u64,
}

/// `FS_IOC_FIEMAP`: `_IOWR('f', 11, struct fiemap)`, where `struct fiemap` is 32 bytes before its extents.
const FS_IOC_FIEMAP: u64 = (3 << 30) | (32 << 16) | ((b'f' as u64) << 8) | 11;
const FIEMAP_EXTENT_LAST: u32 = 0x1;
const FIEMAP_EXTENT_SHARED: u32 = 0x2000;
/// Extents the kernel hasn't placed yet (delayed allocation) or can't say where are: counted, never as shared.
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x2;
const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x200;
/// How many extents one call asks for.
const BATCH: usize = 256;

/// `struct fiemap_extent`.
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

/// `struct fiemap`, with room for [`BATCH`] extents.
#[repr(C)]
struct Fiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
    extents: [Extent; BATCH],
}

/// Measures the files under `path` (a directory reached without symlinks), staying on its device.
pub fn usage(path: &Path) -> Result<TreeUsage> {
    let root = open_dir(path)?;
    let st = fstat(root.as_fd()).map_err(|e| Error::io("reading", path, e))?;
    let mut walk = Walk {
        dev: st.st_dev,
        seen: HashSet::new(),
        out: TreeUsage::default(),
        map: Box::new(Fiemap {
            start: 0,
            length: 0,
            flags: 0,
            mapped_extents: 0,
            extent_count: 0,
            reserved: 0,
            extents: [Extent::default(); BATCH],
        }),
    };
    walk.dir(root, 0).map_err(|e| Error::io("measuring", path, e))?;
    Ok(walk.out)
}

/// The space `ranges` cover, counting overlaps once. Sorts them.
pub fn union_bytes(ranges: &mut [(u64, u64)]) -> u64 {
    ranges.sort_unstable();
    let (mut total, mut end) = (0u64, 0u64);
    for &(start, len) in ranges.iter() {
        let stop = start.saturating_add(len);
        if stop <= end {
            continue;
        }
        total += stop - start.max(end);
        end = stop;
    }
    total
}

/// Deeper than this is skipped rather than followed, so a hostile tree can't run the walk out of descriptors.
const MAX_DEPTH: usize = 256;

struct Walk {
    dev: libc::dev_t,
    seen: HashSet<u64>,
    out: TreeUsage,
    map: Box<Fiemap>,
}

impl Walk {
    fn dir(&mut self, dir: OwnedFd, depth: usize) -> io::Result<()> {
        for name in read_names(dir.as_fd())? {
            let Ok(st) = lstat_at(dir.as_fd(), &name) else {
                self.out.skipped += 1;
                continue;
            };
            if st.st_dev != self.dev {
                continue;
            }
            match st.st_mode & libc::S_IFMT {
                libc::S_IFDIR if depth < MAX_DEPTH => {
                    match openat(
                        dir.as_fd(),
                        &name,
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    ) {
                        Ok(sub) => {
                            if self.dir(sub, depth + 1).is_err() {
                                self.out.skipped += 1;
                            }
                        }
                        Err(_) => self.out.skipped += 1,
                    }
                }
                libc::S_IFDIR => self.out.skipped += 1,
                libc::S_IFREG => {
                    if st.st_nlink > 1 && !self.seen.insert(st.st_ino) {
                        continue;
                    }
                    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY;
                    match openat(dir.as_fd(), &name, flags) {
                        Ok(f) => self.file(f.as_fd(), &st),
                        Err(_) => self.out.skipped += 1,
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn file(&mut self, fd: BorrowedFd, before: &libc::stat) {
        // Swapped for something else since it was looked at: skipped.
        match fstat(fd) {
            Ok(st) if st.st_mode & libc::S_IFMT == libc::S_IFREG && st.st_ino == before.st_ino => {}
            _ => {
                self.out.skipped += 1;
                return;
            }
        }
        let mut start = 0u64;
        loop {
            let m = &mut *self.map;
            m.start = start;
            m.length = u64::MAX - start;
            m.flags = 0;
            m.mapped_extents = 0;
            m.extent_count = BATCH as u32;
            // SAFETY: `m` is a `struct fiemap` with room for `extent_count` extents, as the ioctl expects.
            let r = unsafe { libc::ioctl(fd.as_raw_fd(), FS_IOC_FIEMAP as _, m as *mut Fiemap) };
            if r < 0 {
                if start == 0 {
                    // No FIEMAP here: the allocated blocks, as exclusive.
                    let bytes = (before.st_blocks as u64).saturating_mul(512);
                    self.out.bytes += bytes;
                    self.out.exclusive_bytes += bytes;
                } else {
                    self.out.skipped += 1;
                }
                return;
            }
            let n = (m.mapped_extents as usize).min(BATCH);
            if n == 0 {
                return;
            }
            let mut last = false;
            for e in &m.extents[..n] {
                self.out.bytes += e.length;
                if e.flags & FIEMAP_EXTENT_SHARED != 0
                    && e.flags & (FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DATA_INLINE) == 0
                {
                    self.out.shared.push((e.physical, e.length));
                } else {
                    self.out.exclusive_bytes += e.length;
                }
                last |= e.flags & FIEMAP_EXTENT_LAST != 0;
            }
            let end = m.extents[n - 1].logical.saturating_add(m.extents[n - 1].length);
            if last || end <= start {
                return;
            }
            start = end;
        }
    }
}

/// The names in a directory, without `.` and `..`.
fn read_names(dir: BorrowedFd) -> io::Result<Vec<Vec<u8>>> {
    // `fdopendir` takes the descriptor over, so it gets a copy of its own.
    // SAFETY: `dup` of an open descriptor; the result is checked.
    let fd = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just made, and is owned by the stream from here on.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: still ours, since `fdopendir` failed.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
        return Err(e);
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: `stream` is an open directory stream; the entry is only read before the next call.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `d_name` is a NUL-terminated name inside the entry.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(name.to_vec());
        }
    }
    // SAFETY: closes the stream and its descriptor, once.
    unsafe { libc::closedir(stream) };
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unions_count_overlaps_once() {
        assert_eq!(union_bytes(&mut []), 0);
        assert_eq!(union_bytes(&mut [(0, 10), (10, 5)]), 15);
        assert_eq!(union_bytes(&mut [(0, 10), (0, 10), (5, 10)]), 15);
        assert_eq!(union_bytes(&mut [(100, 10), (0, 10), (2, 3)]), 20);
        assert_eq!(union_bytes(&mut [(0, 100), (10, 10)]), 100);
    }

    #[test]
    fn the_ioctl_number_matches_the_kernel_headers() {
        assert_eq!(FS_IOC_FIEMAP, 0xC020_660B);
        assert_eq!(size_of::<Extent>(), 56);
        assert_eq!(std::mem::offset_of!(Fiemap, extents), 32);
    }
}
