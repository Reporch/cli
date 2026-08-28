// Path resolution and file I/O for the ext4 reader.
//
// Adds high-level operations (`exists`, `stat`, `list_dir`, `read_file`) on
// top of the low-level [`Reader`] built in `reader.rs`.  Path resolution
// follows POSIX semantics including symlink traversal with loop detection.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

use crate::constants::*;
use crate::error::{ReadError, ReadResult};
use crate::extent;
use crate::file_tree::InodeNumber;
use crate::reader::Reader;
use crate::types::*;

/// Maximum number of symlink hops before we declare a loop.
const MAX_SYMLINK_HOPS: usize = 40;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl Reader {
    /// Check whether `path` exists in this ext4 filesystem.
    pub fn exists(&mut self, path: &str) -> bool {
        self.resolve_path(path, true).is_ok()
    }

    /// Return `(inode_number, inode)` for the given `path`, following symlinks.
    pub fn stat(&mut self, path: &str) -> ReadResult<(InodeNumber, Inode)> {
        self.resolve_path(path, true)
    }

    /// Return `(inode_number, inode)` for the given `path` **without** following
    /// the final symlink component.
    pub fn stat_no_follow(&mut self, path: &str) -> ReadResult<(InodeNumber, Inode)> {
        self.resolve_path(path, false)
    }

    /// List a directory's entries (names only, excluding "." and "..").
    pub fn list_dir(&mut self, path: &str) -> ReadResult<Vec<String>> {
        let (ino_num, inode) = self.stat(path)?;
        if !inode.is_dir() {
            return Err(ReadError::NotADirectory(path.to_string()));
        }
        let children = self.children_of(ino_num)?;
        Ok(children
            .into_iter()
            .filter(|(name, _)| name != "." && name != "..")
            .map(|(name, _)| name)
            .collect())
    }

    /// Read file contents at `path` starting at `offset`.
    ///
    /// If `count` is `None`, reads to EOF.  Returns the bytes read (may be
    /// shorter than requested if the file is smaller).
    pub fn read_file(
        &mut self,
        path: &str,
        offset: u64,
        count: Option<usize>,
    ) -> ReadResult<Vec<u8>> {
        let (ino_num, inode) = self.stat(path)?;

        if inode.is_dir() {
            return Err(ReadError::IsDirectory(path.to_string()));
        }
        if !inode.is_reg() {
            return Err(ReadError::NotAFile(path.to_string()));
        }

        let file_size = inode.file_size();
        let start = offset.min(file_size);
        let max_readable = file_size - start;
        let want = match count {
            Some(c) => (c as u64).min(max_readable),
            None => max_readable,
        };

        if want == 0 {
            return Ok(Vec::new());
        }

        self.read_from_extents(ino_num, start, want)
    }

    /// Read file contents into a pre-allocated buffer.  Returns the number of
    /// bytes actually written to `buf` (may be less than `buf.len()` at EOF).
    pub fn read_file_into(
        &mut self,
        path: &str,
        buf: &mut [u8],
        offset: u64,
    ) -> ReadResult<usize> {
        let data = self.read_file(path, offset, Some(buf.len()))?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

impl Reader {
    /// Resolve `path` to an inode, optionally following symlinks.
    ///
    /// * Absolute paths start from the root inode.
    /// * Relative paths also start from the root (there is no "cwd").
    /// * "." is a no-op, ".." goes to the parent (via a parent stack).
    /// * Symlinks are recursively resolved up to [`MAX_SYMLINK_HOPS`].
    fn resolve_path(
        &mut self,
        path: &str,
        follow_symlinks: bool,
    ) -> ReadResult<(InodeNumber, Inode)> {
        let mut components = normalize_path(path);
        let mut current: InodeNumber = ROOT_INODE;
        let mut parent_stack: Vec<InodeNumber> = Vec::new();
        let mut visited: HashSet<InodeNumber> = HashSet::new();
        let mut hops: usize = 0;
        let mut idx: usize = 0;

        while idx < components.len() {
            let name = components[idx].clone();

            // "." -- stay in the current directory.
            if name == "." {
                idx += 1;
                continue;
            }

            // ".." -- go to parent.
            if name == ".." {
                if current != ROOT_INODE {
                    if let Some(parent) = parent_stack.pop() {
                        current = parent;
                    } else {
                        // Fallback: look up ".." in the directory entries.
                        let entries = self.children_of(current)?;
                        if let Some((_, parent_ino)) = entries.iter().find(|(n, _)| n == "..") {
                            current = *parent_ino;
                        }
                    }
                }
                idx += 1;
                continue;
            }

            // Regular component -- current must be a directory.
            let current_inode = self.get_inode(current)?;
            if !current_inode.is_dir() {
                return Err(ReadError::NotADirectory(name));
            }

            // Look up the child by name.
            let entries = self.children_of(current)?;
            let child_ino = entries
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, ino)| *ino)
                .ok_or_else(|| ReadError::PathNotFound(name.clone()))?;

            let child_inode = self.get_inode(child_ino)?;

            if child_inode.is_link() && follow_symlinks {
                // Symlink loop detection.
                if visited.contains(&child_ino) {
                    return Err(ReadError::SymlinkLoop(path.to_string()));
                }
                visited.insert(child_ino);

                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(ReadError::SymlinkLoop(path.to_string()));
                }

                // Read the symlink target.
                let target = self.read_symlink_target(&child_inode, child_ino)?;
                if target.is_empty() {
                    return Err(ReadError::InvalidPath("empty symlink target".to_string()));
                }

                let target_components = normalize_path(&target);

                if target.starts_with('/') {
                    // Absolute symlink: restart from root.
                    current = ROOT_INODE;
                    parent_stack.clear();
                    // Replace all remaining components with target + rest.
                    let rest: Vec<String> = components[idx + 1..].to_vec();
                    components = [target_components, rest].concat();
                    idx = 0;
                } else {
                    // Relative symlink: splice target into the component list.
                    let before: Vec<String> = components[..idx].to_vec();
                    let rest: Vec<String> = components[idx + 1..].to_vec();
                    components = [before, target_components, rest].concat();
                    // Do not advance idx -- re-process from the splice point.
                }
            } else {
                // Not a symlink (or not following) -- descend.
                parent_stack.push(current);
                current = child_ino;
                idx += 1;
            }
        }

        let final_inode = self.get_inode(current)?;
        Ok((current, final_inode))
    }
}

// ---------------------------------------------------------------------------
// File reading from extents
// ---------------------------------------------------------------------------

impl Reader {
    /// Read `count` bytes starting at byte offset `start` from the file
    /// described by `inode_number`'s extent tree.
    fn read_from_extents(
        &mut self,
        inode_number: InodeNumber,
        start: u64,
        count: u64,
    ) -> ReadResult<Vec<u8>> {
        let inode = self.get_inode(inode_number)?;
        let extents = extent::parse_extents(&inode, self.block_size(), &mut self.file)?;

        if extents.is_empty() {
            return Ok(Vec::new());
        }

        let bs = self.block_size();
        let req_end = start + count;
        let mut out = Vec::with_capacity(count as usize);
        let mut logical_offset: u64 = 0;

        for (phys_start, phys_end) in &extents {
            let extent_bytes = (*phys_end as u64 - *phys_start as u64) * bs;
            let logical_end = logical_offset + extent_bytes;

            // Skip extents entirely before the requested range.
            if logical_end <= start {
                logical_offset = logical_end;
                continue;
            }
            // Stop once we have passed the requested range.
            if logical_offset >= req_end {
                break;
            }

            // Compute the overlap between the requested range and this extent.
            let overlap_start = start.max(logical_offset);
            let overlap_end = req_end.min(logical_end);
            let mut remaining = overlap_end - overlap_start;

            if remaining == 0 {
                logical_offset = logical_end;
                continue;
            }

            // Seek to the correct byte within this extent.
            let offset_into_extent = overlap_start - logical_offset;
            let abs_byte_offset = *phys_start as u64 * bs + offset_into_extent;
            self.file.seek(SeekFrom::Start(abs_byte_offset))?;

            // Read in chunks of up to 1 MiB to avoid enormous single reads.
            while remaining > 0 {
                let chunk = remaining.min(1 << 20) as usize;
                let mut buf = vec![0u8; chunk];
                let n = self.file.read(&mut buf)?;
                if n == 0 {
                    break; // EOF
                }
                out.extend_from_slice(&buf[..n]);
                remaining -= n as u64;
            }

            logical_offset = logical_end;
            if out.len() >= count as usize {
                break;
            }
        }

        // Truncate to exactly `count` bytes in case we over-read.
        out.truncate(count as usize);
        Ok(out)
    }

    /// Read the target of a symbolic link.
    ///
    /// Fast symlinks (< 60 bytes) store the target directly in the inode's
    /// `block` field.  Longer targets are stored in data blocks referenced by
    /// the inode's extent tree.
    fn read_symlink_target(
        &mut self,
        inode: &Inode,
        inode_number: InodeNumber,
    ) -> ReadResult<String> {
        let size = inode.file_size();
        if size == 0 {
            return Ok(String::new());
        }

        if size < INODE_BLOCK_SIZE as u64 {
            // Fast symlink: target is stored inline in the block field.
            let bytes = &inode.block[..size as usize];
            return Ok(String::from_utf8_lossy(bytes).into_owned());
        }

        // Slow symlink: read from extents.
        let data = self.read_from_extents(inode_number, 0, size)?;
        Ok(String::from_utf8_lossy(&data).into_owned())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Normalize a path into components, stripping leading "/" and splitting on
/// "/".  Empty components (from consecutive slashes) are dropped.
fn normalize_path(path: &str) -> Vec<String> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split('/')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path_absolute() {
        assert_eq!(normalize_path("/etc/passwd"), vec!["etc", "passwd"]);
    }

    #[test]
    fn test_normalize_path_relative() {
        assert_eq!(normalize_path("etc/passwd"), vec!["etc", "passwd"]);
    }

    #[test]
    fn test_normalize_path_root() {
        assert!(normalize_path("/").is_empty());
    }

    #[test]
    fn test_normalize_path_empty() {
        assert!(normalize_path("").is_empty());
    }

    #[test]
    fn test_normalize_path_consecutive_slashes() {
        assert_eq!(normalize_path("//a///b//"), vec!["a", "b"]);
    }

    #[test]
    fn test_normalize_path_dots() {
        // Dots are kept as components; resolution handles them during traversal.
        assert_eq!(
            normalize_path("/a/./b/../c"),
            vec!["a", ".", "b", "..", "c"]
        );
    }

    #[test]
    fn test_normalize_path_single_component() {
        assert_eq!(normalize_path("/file.txt"), vec!["file.txt"]);
        assert_eq!(normalize_path("file.txt"), vec!["file.txt"]);
    }

    #[test]
    fn test_normalize_path_trailing_slash() {
        // A trailing slash should not produce an empty trailing component.
        assert_eq!(normalize_path("/etc/"), vec!["etc"]);
        assert_eq!(normalize_path("/a/b/c/"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_normalize_path_deeply_nested() {
        assert_eq!(
            normalize_path("/a/b/c/d/e/f"),
            vec!["a", "b", "c", "d", "e", "f"]
        );
    }
}
