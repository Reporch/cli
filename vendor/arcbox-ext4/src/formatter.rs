// Core ext4 image formatter.
//
// Creates ext4 filesystem images from scratch using a single-pass sequential
// write strategy.  The public entry point is `Formatter::new()` followed by
// repeated `create()` / `link()` / `unlink()` calls, finalized with `close()`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use uuid::Uuid;

use crate::constants::*;
use crate::dir;
use crate::error::{FormatError, FormatResult};
use crate::extent;
use crate::file_tree::{BlockRange, FileTree, FileTreeNode, InodeNumber};
use crate::types::*;
use crate::xattr::{ExtendedAttribute, XattrState};

/// Maximum bytes that fit in the ext4 superblock's `volume_name` field.
const VOLUME_NAME_LEN: usize = 16;

// ---------------------------------------------------------------------------
// FormatOptions
// ---------------------------------------------------------------------------

/// Parameters for creating a new ext4 image.
///
/// Construct via [`FormatOptions::new`] and layer on [`uuid`] / [`label`] as
/// needed. The resulting options are passed to [`Formatter::with_options`].
///
/// [`uuid`]: Self::uuid
/// [`label`]: Self::label
#[derive(Clone, Debug)]
pub struct FormatOptions {
    /// Block size in bytes. Only 4096 is currently accepted; the field is
    /// explicit so future versions can widen the accepted set without another
    /// API break.
    pub block_size: u32,
    /// Total size of the image file in bytes.
    pub size: u64,
    /// UUID written to the superblock. `None` picks a random v4 UUID at
    /// format time.
    pub uuid: Option<Uuid>,
    /// Volume label. Must be ≤ 16 bytes UTF-8 and contain no NUL bytes.
    /// `None` leaves the field zeroed.
    pub label: Option<String>,
}

impl FormatOptions {
    /// Create options for an image of the given size, with default
    /// `block_size = 4096`, random UUID, no label.
    pub fn new(size: u64) -> Self {
        Self {
            block_size: 4096,
            size,
            uuid: None,
            label: None,
        }
    }

    /// Set an explicit UUID instead of a randomly generated one.
    pub fn uuid(mut self, uuid: Uuid) -> Self {
        self.uuid = Some(uuid);
        self
    }

    /// Set the volume label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

fn validate_label(label: &str) -> FormatResult<()> {
    if label.as_bytes().len() > VOLUME_NAME_LEN {
        return Err(FormatError::InvalidLabel(format!(
            "label {label:?} is {} bytes, ext4 limit is {VOLUME_NAME_LEN}",
            label.as_bytes().len()
        )));
    }
    if label.contains('\0') {
        return Err(FormatError::InvalidLabel(format!(
            "label {label:?} contains a NUL byte"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FileTimestamps
// ---------------------------------------------------------------------------

/// Timestamps to apply to a newly created inode.
///
/// Each pair is `(seconds_lo, extra)` in ext4 format -- the same layout
/// returned by `timestamp_now()`.
pub struct FileTimestamps {
    pub access_lo: u32,
    pub access_hi: u32,
    pub modification_lo: u32,
    pub modification_hi: u32,
    pub creation_lo: u32,
    pub creation_hi: u32,
    pub now_lo: u32,
    pub now_hi: u32,
}

impl Default for FileTimestamps {
    fn default() -> Self {
        let (lo, hi) = timestamp_now();
        Self {
            access_lo: lo,
            access_hi: hi,
            modification_lo: lo,
            modification_hi: hi,
            creation_lo: lo,
            creation_hi: hi,
            now_lo: lo,
            now_hi: hi,
        }
    }
}

// ---------------------------------------------------------------------------
// Formatter
// ---------------------------------------------------------------------------

/// Single-pass ext4 image builder.
pub struct Formatter {
    file: File,
    block_size: u32,
    size: u64,
    inodes: Vec<Inode>,
    tree: FileTree,
    deleted_blocks: Vec<BlockRange>,
    /// Block ranges owned by inodes that still have links_count > 0 but
    /// whose original tree node has been detached.  Keyed by inode number.
    /// Used to reclaim blocks when the last hard-link reference is removed.
    deferred_blocks: HashMap<u32, Vec<BlockRange>>,
    /// UUID to write into the superblock. `None` triggers a random v4 at
    /// format time.
    uuid: Option<Uuid>,
    /// Volume label to copy into the superblock's `volume_name` field. Validated
    /// on construction.
    label: Option<String>,
}

impl Formatter {
    // -- Computed properties ------------------------------------------------

    #[inline]
    fn blocks_per_group(&self) -> u32 {
        self.block_size * 8
    }

    #[inline]
    fn max_inodes_per_group(&self) -> u32 {
        self.block_size * 8
    }

    #[inline]
    fn groups_per_descriptor_block(&self) -> u32 {
        self.block_size / 32
    }

    #[inline]
    fn block_count(&self) -> u32 {
        ((self.size - 1) / self.block_size as u64 + 1) as u32
    }

    #[inline]
    fn group_count(&self) -> u32 {
        (self.block_count() - 1) / self.blocks_per_group() + 1
    }

    #[inline]
    fn group_descriptor_blocks(&self) -> u32 {
        ((self.group_count() - 1) / self.groups_per_descriptor_block() + 1) * 32
    }

    #[inline]
    fn pos(&mut self) -> u64 {
        self.file.stream_position().unwrap_or(0)
    }

    #[inline]
    fn current_block(&mut self) -> u32 {
        let p = self.pos();
        (p / self.block_size as u64) as u32
    }

    fn seek_to_block(&mut self, block: u32) -> io::Result<()> {
        self.file
            .seek(SeekFrom::Start(block as u64 * self.block_size as u64))?;
        Ok(())
    }

    fn align_to_block(&mut self) -> io::Result<()> {
        let p = self.pos();
        if p % self.block_size as u64 != 0 {
            let blk = self.current_block() + 1;
            self.seek_to_block(blk)?;
        }
        Ok(())
    }

    // -- Constructor -------------------------------------------------------

    /// Create a new formatter that writes an ext4 image to `path`.
    ///
    /// The file is truncated and re-created as a sparse file of `opts.size`
    /// bytes. The root directory (inode 2) and the `/lost+found` directory
    /// (required by e2fsck) are created automatically.
    ///
    /// Only `opts.block_size == 4096` is currently accepted; other values
    /// return [`FormatError::UnsupportedBlockSize`]. Labels are validated
    /// eagerly — an oversize or NUL-containing label returns
    /// [`FormatError::InvalidLabel`] before any file work happens.
    pub fn with_options(path: &Path, opts: FormatOptions) -> FormatResult<Self> {
        // Only 4096-byte blocks are supported.  Supporting smaller block sizes
        // (1024, 2048) would require first_data_block=1 and different group
        // descriptor offset calculations throughout the formatter and reader.
        if opts.block_size != 4096 {
            return Err(FormatError::UnsupportedBlockSize(opts.block_size));
        }

        if let Some(label) = &opts.label {
            validate_label(label)?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        // Create a sparse file.
        file.set_len(opts.size)?;

        // Reserve the first 10 inodes (indices 0..9  =>  inode numbers 1..10).
        //   [0] = defective block inode  (empty / default)
        //   [1] = root inode             (inode 2)
        //   [2..9] = reserved            (empty / default)
        let mut inodes = Vec::with_capacity(16);
        inodes.push(Inode::default()); // inode 1 -- defective blocks
        inodes.push(Inode::root_inode()); // inode 2 -- root directory
        for _ in 2..10 {
            inodes.push(Inode::default()); // inodes 3..10
        }

        let tree = FileTree::new(ROOT_INODE, "/");

        let mut fmt = Self {
            file,
            block_size: opts.block_size,
            size: opts.size,
            inodes,
            tree,
            deleted_blocks: Vec::new(),
            deferred_blocks: HashMap::new(),
            uuid: opts.uuid,
            label: opts.label,
        };

        // Seek past the superblock (block 0) and the group descriptor table.
        let gdb = fmt.group_descriptor_blocks();
        fmt.seek_to_block(gdb + 1)?;

        // /lost+found is required by e2fsck.
        fmt.create(
            "/lost+found",
            make_mode(file_mode::S_IFDIR, 0o700),
            None,
            None,
            None,
            None,
            None,
            None,
        )?;

        Ok(fmt)
    }

    /// Create a new formatter with defaults for UUID (random) and label (empty).
    ///
    /// Thin shim over [`Formatter::with_options`] for the common case where
    /// only the image size matters. Callers that need a specific UUID or
    /// volume label should use `with_options` directly.
    pub fn new(path: &Path, block_size: u32, min_disk_size: u64) -> FormatResult<Self> {
        Self::with_options(
            path,
            FormatOptions {
                block_size,
                size: min_disk_size,
                uuid: None,
                label: None,
            },
        )
    }

    // -- create() ----------------------------------------------------------

    /// Create a file, directory, or symlink at `path`.
    ///
    /// Parent directories are created recursively with mode 0755, inheriting
    /// uid/gid from the nearest existing parent (like `mkdir -p`).
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &mut self,
        path: &str,
        mode: u16,
        link: Option<&str>,
        ts: Option<FileTimestamps>,
        data: Option<&mut dyn Read>,
        uid: Option<u32>,
        gid: Option<u32>,
        xattrs: Option<&HashMap<String, Vec<u8>>>,
    ) -> FormatResult<()> {
        let path_buf = std::path::PathBuf::from(path);

        // ------- Handle "already exists" cases -------
        if let Some(existing_idx) = self.tree.lookup(&path_buf) {
            let existing_inode_num = self.tree.node(existing_idx).inode;
            let existing_inode = &self.inodes[(existing_inode_num - 1) as usize];

            if is_dir(mode) {
                if existing_inode.is_dir() {
                    // mkdir -p: directory already exists.  Only update mode
                    // and ownership when the caller explicitly provided them
                    // (uid/gid != None).  This prevents recursive parent-
                    // creation from silently downgrading permissions set by
                    // an earlier explicit create() call.
                    if self.tree.node(existing_idx).name == basename(path) {
                        if uid.is_some() || gid.is_some() {
                            let inode =
                                &mut self.inodes[(existing_inode_num - 1) as usize];
                            inode.mode = mode;
                            if let Some(u) = uid {
                                inode.set_uid(u);
                            }
                            if let Some(g) = gid {
                                inode.set_gid(g);
                            }
                        }
                        return Ok(());
                    }
                } else {
                    // A regular file (or other non-dir) blocks directory creation.
                    return Err(FormatError::NotDirectory(path_buf));
                }
            } else if self.tree.node(existing_idx).link.is_some() {
                // Hard-link entries can always be overwritten.
                self.unlink(path, false)?;
            } else {
                // File or symlink replacing an existing entry.
                if existing_inode.is_dir() && !is_link(mode) {
                    return Err(FormatError::NotFile(path_buf));
                }
                self.unlink(path, false)?;
            }
        }

        // ------- Ensure parent directories exist -------
        let parent_path = parent_of(path);
        if parent_path != path {
            self.create(
                parent_path,
                make_mode(file_mode::S_IFDIR, 0o755),
                None,
                None,
                None,
                None,
                None,
                None,
            )?;
        }

        let parent_path_buf = std::path::PathBuf::from(parent_path);
        let parent_idx = self
            .tree
            .lookup(&parent_path_buf)
            .ok_or_else(|| FormatError::NotFound(parent_path_buf.clone()))?;
        let parent_inode_num = self.tree.node(parent_idx).inode;
        let parent_inode = &self.inodes[(parent_inode_num - 1) as usize];

        if parent_inode.links_count as u32 >= MAX_LINKS {
            return Err(FormatError::MaximumLinksExceeded(parent_path_buf));
        }

        // ------- Build the child inode -------
        let mut child_inode = Inode::default();
        child_inode.mode = mode;

        // uid / gid -- inherit from parent when not specified.
        if let Some(u) = uid {
            child_inode.set_uid(u);
        } else {
            child_inode.uid = parent_inode.uid;
            child_inode.uid_hi = parent_inode.uid_hi;
        }
        if let Some(g) = gid {
            child_inode.set_gid(g);
        } else {
            child_inode.gid = parent_inode.gid;
            child_inode.gid_hi = parent_inode.gid_hi;
        }

        // Extended attributes.
        if let Some(xattr_map) = xattrs {
            if !xattr_map.is_empty() {
                let child_ino = self.inodes.len() as u32 + 1;
                let mut state =
                    XattrState::new(child_ino, INODE_EXTRA_SIZE, self.block_size);
                // The reference implementation adds a sentinel "system.data" attribute.
                state.add(ExtendedAttribute::new("system.data", Vec::new()))?;
                for (name, value) in xattr_map {
                    state.add(ExtendedAttribute::new(name, value.clone()))?;
                }
                if state.has_inline() {
                    let buf = state.write_inline()?;
                    child_inode.inline_xattrs.copy_from_slice(&buf);
                }
                if state.has_block() {
                    let block_buf = state.write_block()?;
                    self.align_to_block()?;
                    child_inode.xattr_block_lo = self.current_block();
                    self.file.write_all(&block_buf)?;
                    // Account for the xattr block.  When HUGE_FILE is set,
                    // blocks_lo is in filesystem-block units; otherwise sectors.
                    if child_inode.flags & inode_flags::HUGE_FILE != 0 {
                        child_inode.blocks_lo += 1;
                    } else {
                        child_inode.blocks_lo += (self.block_size / 512) as u32;
                    }
                }
            }
        }

        // Timestamps.
        let ts = ts.unwrap_or_default();
        child_inode.atime = ts.access_lo;
        child_inode.atime_extra = ts.access_hi;
        child_inode.ctime = ts.now_lo;
        child_inode.ctime_extra = ts.now_hi;
        child_inode.mtime = ts.modification_lo;
        child_inode.mtime_extra = ts.modification_hi;
        child_inode.crtime = ts.creation_lo;
        child_inode.crtime_extra = ts.creation_hi;
        child_inode.links_count = 1;
        child_inode.extra_isize = EXTRA_ISIZE;
        child_inode.flags = inode_flags::HUGE_FILE;

        // Align to block boundary before writing data.
        self.align_to_block()?;

        let mut start_block: u32 = 0;
        let mut end_block: u32 = 0;

        // ------- Handle by file type -------
        if is_dir(mode) {
            // Directory: bump link counts; directory entries are deferred to
            // close() so they can be sorted.
            child_inode.links_count = 2;
            self.inodes[(parent_inode_num - 1) as usize].links_count += 1;
        } else if let Some(link_target) = link {
            // Symbolic link.
            start_block = self.current_block();
            let link_bytes = link_target.as_bytes();

            let size = if link_bytes.len() < 60 {
                // Short symlink: store inline in the inode's block field.
                child_inode.block[..link_bytes.len()].copy_from_slice(link_bytes);
                link_bytes.len() as u64
            } else {
                // Long symlink: write to data blocks.
                self.file.write_all(link_bytes)?;
                link_bytes.len() as u64
            };

            self.align_to_block()?;
            end_block = self.current_block();

            child_inode.set_file_size(size);
            child_inode.mode |= 0o777;
            child_inode.flags = 0;

            if link_bytes.len() < 60 {
                child_inode.blocks_lo = 0;
            } else {
                let blocks = BlockRange {
                    start: start_block,
                    end: end_block,
                };
                let mut cur = self.current_block();
                extent::write_extents(
                    &mut child_inode,
                    blocks,
                    self.block_size,
                    &mut self.file,
                    &mut cur,
                )?;
            }
        } else if is_reg(mode) {
            // Regular file.
            start_block = self.current_block();
            let mut size = 0u64;

            if let Some(reader) = data {
                let mut buf = vec![0u8; self.block_size as usize];
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    size += n as u64;
                    if size > MAX_FILE_SIZE {
                        return Err(FormatError::FileTooBig(size));
                    }
                    self.file.write_all(&buf[..n])?;
                }
            }

            self.align_to_block()?;
            end_block = self.current_block();

            child_inode.set_file_size(size);

            let blocks = BlockRange {
                start: start_block,
                end: end_block,
            };
            let mut cur = self.current_block();
            extent::write_extents(
                &mut child_inode,
                blocks,
                self.block_size,
                &mut self.file,
                &mut cur,
            )?;
        } else {
            return Err(FormatError::UnsupportedFiletype);
        }

        // ------- Register the new inode and tree node -------
        self.inodes.push(child_inode);
        let child_inode_num = self.inodes.len() as InodeNumber;

        let child_node = FileTreeNode {
            inode: child_inode_num,
            name: basename(path).to_string(),
            children: Vec::new(),
            parent: None,
            blocks: if start_block != end_block {
                Some(BlockRange {
                    start: start_block,
                    end: end_block,
                })
            } else {
                None
            },
            additional_blocks: Vec::new(),
            link: None,
        };
        self.tree.add_child(parent_idx, child_node);

        Ok(())
    }

    // -- Query helpers (for Ext4Writer / layer.rs) --------------------------

    /// Check whether `path` exists in the in-memory file tree.
    pub fn exists(&self, path: &str) -> bool {
        self.tree.lookup(&std::path::PathBuf::from(path)).is_some()
    }

    /// Check whether `path` exists and is a directory.
    pub fn is_dir(&self, path: &str) -> bool {
        let path_buf = std::path::PathBuf::from(path);
        if let Some(idx) = self.tree.lookup(&path_buf) {
            let ino = self.tree.node(idx).inode;
            self.inodes[(ino - 1) as usize].is_dir()
        } else {
            false
        }
    }

    /// List the names of immediate children of a directory (excluding `.`
    /// and `..`).  Returns an empty vec if `path` is not a directory.
    pub fn list_dir(&self, path: &str) -> Vec<String> {
        let path_buf = std::path::PathBuf::from(path);
        let Some(idx) = self.tree.lookup(&path_buf) else {
            return Vec::new();
        };
        self.tree
            .node(idx)
            .children
            .iter()
            .map(|&ci| self.tree.node(ci).name.clone())
            .collect()
    }

    /// Update the permission bits of an existing entry.
    pub fn set_permissions(&mut self, path: &str, mode: u16) -> FormatResult<()> {
        let path_buf = std::path::PathBuf::from(path);
        let idx = self
            .tree
            .lookup(&path_buf)
            .ok_or_else(|| FormatError::NotFound(path_buf))?;
        let ino = self.tree.node(idx).inode;
        let inode = &mut self.inodes[(ino - 1) as usize];
        // Preserve the file-type bits, replace the permission bits.
        inode.mode = (inode.mode & file_mode::TYPE_MASK) | (mode & !file_mode::TYPE_MASK);
        Ok(())
    }

    /// Update the owner uid/gid of an existing entry.
    pub fn set_owner(&mut self, path: &str, uid: u32, gid: u32) -> FormatResult<()> {
        let path_buf = std::path::PathBuf::from(path);
        let idx = self
            .tree
            .lookup(&path_buf)
            .ok_or_else(|| FormatError::NotFound(path_buf))?;
        let ino = self.tree.node(idx).inode;
        let inode = &mut self.inodes[(ino - 1) as usize];
        inode.set_uid(uid);
        inode.set_gid(gid);
        Ok(())
    }

    // -- link() ------------------------------------------------------------

    /// Create a hard link at `link_path` pointing to `target_path`.
    pub fn link(&mut self, link_path: &str, target_path: &str) -> FormatResult<()> {
        let target_buf = std::path::PathBuf::from(target_path);
        let target_idx = self
            .tree
            .lookup(&target_buf)
            .ok_or_else(|| FormatError::NotFound(target_buf.clone()))?;
        let target_inode_num = self.tree.node(target_idx).inode;
        let target_inode = &self.inodes[(target_inode_num - 1) as usize];

        if target_inode.is_dir() {
            return Err(FormatError::CannotHardlinkDirectory(target_buf));
        }

        // Increment link count on the target inode.
        self.inodes[(target_inode_num - 1) as usize].links_count += 1;

        // If the link path already exists, remove it first.
        let link_buf = std::path::PathBuf::from(link_path);
        if self.tree.lookup(&link_buf).is_some() {
            self.unlink(link_path, false)?;
        }

        let parent_path = parent_of(link_path);
        let parent_path_buf = std::path::PathBuf::from(parent_path);
        let parent_idx = self
            .tree
            .lookup(&parent_path_buf)
            .ok_or_else(|| FormatError::NotFound(parent_path_buf.clone()))?;

        let parent_inode_num = self.tree.node(parent_idx).inode;
        if self.inodes[(parent_inode_num - 1) as usize].links_count as u32 >= MAX_LINKS {
            return Err(FormatError::MaximumLinksExceeded(parent_path_buf));
        }

        let link_node = FileTreeNode {
            inode: ROOT_INODE, // placeholder, not used for hard links
            name: basename(link_path).to_string(),
            children: Vec::new(),
            parent: None,
            blocks: None,
            additional_blocks: Vec::new(),
            link: Some(target_inode_num),
        };
        self.tree.add_child(parent_idx, link_node);

        Ok(())
    }

    // -- unlink() ----------------------------------------------------------

    /// Remove the entry at `path`.
    ///
    /// When `directory_whiteout` is true, only children are removed and the
    /// directory entry itself is kept (used for overlay whiteouts).
    pub fn unlink(&mut self, path: &str, directory_whiteout: bool) -> FormatResult<()> {
        let path_buf = std::path::PathBuf::from(path);
        let node_idx = match self.tree.lookup(&path_buf) {
            Some(idx) => idx,
            None => return Ok(()), // nothing to unlink
        };

        let inode_num = self.tree.node(node_idx).inode;
        let inode_idx = (inode_num - 1) as usize;

        if directory_whiteout && !self.inodes[inode_idx].is_dir() {
            return Err(FormatError::NotDirectory(path_buf));
        }

        // Recursively unlink children.
        let child_names: Vec<String> = self
            .tree
            .node(node_idx)
            .children
            .iter()
            .map(|&ci| self.tree.node(ci).name.clone())
            .collect();
        for child_name in child_names {
            let child_path = if path == "/" {
                format!("/{child_name}")
            } else {
                format!("{path}/{child_name}")
            };
            self.unlink(&child_path, false)?;
        }

        if directory_whiteout {
            return Ok(());
        }

        // Detach from parent and update parent's link count for directories.
        let parent_path = parent_of(path);
        let parent_path_buf = std::path::PathBuf::from(parent_path);
        if let Some(parent_idx) = self.tree.lookup(&parent_path_buf) {
            let parent_inode_num = self.tree.node(parent_idx).inode;
            if self.inodes[inode_idx].is_dir()
                && self.inodes[(parent_inode_num - 1) as usize].links_count > 2
            {
                self.inodes[(parent_inode_num - 1) as usize].links_count -= 1;
            }
            self.tree.remove_child(parent_idx, basename(path));
        }

        // Determine which inode to decrement and potentially reclaim.
        // For hard-link entries the target inode is the one that matters;
        // for regular entries it is the node's own inode.
        let (target_ino, target_idx) =
            if let Some(linked_ino) = self.tree.node(node_idx).link {
                (linked_ino, (linked_ino - 1) as usize)
            } else {
                (inode_num, inode_idx)
            };

        if target_ino > FIRST_INODE - 1 {
            if self.inodes[target_idx].links_count > 0 {
                self.inodes[target_idx].links_count -= 1;
            }

            // Collect block ranges from this tree node (if any).
            let node = self.tree.node(node_idx);
            let mut node_blocks: Vec<BlockRange> = Vec::new();
            if let Some(b) = node.blocks {
                if b.start != b.end {
                    node_blocks.push(b);
                }
            }
            for blk in &node.additional_blocks {
                node_blocks.push(*blk);
            }

            if self.inodes[target_idx].links_count == 0 {
                // Last reference removed -- reclaim blocks and mark deleted.
                // Blocks may come from this node (if it is the original) or
                // from deferred_blocks (if the original was unlinked earlier
                // while hard links still existed).
                for blk in &node_blocks {
                    self.deleted_blocks.push(*blk);
                }
                if let Some(deferred) = self.deferred_blocks.remove(&target_ino) {
                    for blk in deferred {
                        self.deleted_blocks.push(blk);
                    }
                }

                let (now_lo, _) = timestamp_now();
                self.inodes[target_idx] = Inode::default();
                self.inodes[target_idx].dtime = now_lo;
            } else if !node_blocks.is_empty() {
                // The original node is being removed but the inode still has
                // remaining hard-link references.  Stash the block ranges so
                // they can be reclaimed when the last link is finally deleted.
                self.deferred_blocks
                    .entry(target_ino)
                    .or_default()
                    .extend(node_blocks);
            }
        }

        Ok(())
    }

    // -- close() -----------------------------------------------------------

    /// Finalize the ext4 image.
    ///
    /// Writes directory entries (BFS order), the inode table, block/inode
    /// bitmaps, group descriptors, and the superblock.  Consumes `self`.
    pub fn close(mut self) -> FormatResult<()> {
        // -- Step 1: BFS-commit directory entries --
        self.commit_directories()?;

        // -- Step 2: Optimize block group layout --
        let current_blk = self.current_block();
        let inode_count = self.inodes.len() as u32;
        let (block_groups, inodes_per_group) =
            self.optimize_block_group_layout(current_blk, inode_count);

        // -- Step 3: Write inode table --
        let inode_table_offset = self.commit_inode_table(block_groups, inodes_per_group)?;

        self.align_to_block()?;

        // -- Step 4: Write bitmaps and build group descriptors --
        let bitmap_offset = self.current_block();
        let bitmap_size = block_groups * 2; // block bitmap + inode bitmap per group
        let data_size = bitmap_offset + bitmap_size;

        // Ensure the disk is large enough.
        let minimum_disk_size = if block_groups == 1 {
            self.blocks_per_group()
        } else {
            (block_groups - 1) * self.blocks_per_group() + 1
        };

        if self.size < minimum_disk_size as u64 * self.block_size as u64 {
            self.size = minimum_disk_size as u64 * self.block_size as u64;
        }

        let inode_table_size_per_group = inodes_per_group * INODE_SIZE / self.block_size;

        // Check if we need more groups for the full disk size.
        let min_groups =
            ((self.pos() / self.block_size as u64) - 1) / self.blocks_per_group() as u64 + 1;
        if self.size < min_groups * self.blocks_per_group() as u64 * self.block_size as u64 {
            self.size = min_groups * self.blocks_per_group() as u64 * self.block_size as u64;
            let saved_pos = self.pos();
            self.file.set_len(self.size)?;
            self.file.seek(SeekFrom::Start(saved_pos))?;
        }

        let total_groups =
            ((self.size / self.block_size as u64) - 1) / self.blocks_per_group() as u64 + 1;

        // Align disk size to block-group boundary.
        if self.size < total_groups * self.blocks_per_group() as u64 * self.block_size as u64 {
            self.size = total_groups * self.blocks_per_group() as u64 * self.block_size as u64;
            let saved_pos = self.pos();
            self.file.set_len(self.size)?;
            self.file.seek(SeekFrom::Start(saved_pos))?;
        }

        let total_groups_u32 = total_groups as u32;

        // Verify group descriptor space.
        let gd_block_count =
            (total_groups_u32 - 1) / self.groups_per_descriptor_block() + 1;
        if gd_block_count > self.group_descriptor_blocks() {
            return Err(FormatError::InsufficientSpaceForGroupDescriptorBlocks);
        }

        let mut total_used_blocks: u32 = 0;
        let mut total_used_inodes: u32 = 0;
        let mut group_descriptors = Vec::with_capacity(total_groups_u32 as usize);

        // Write bitmaps for groups that contain data.
        for group in 0..block_groups {
            let mut dirs: u32 = 0;
            let mut used_inodes: u32 = 0;
            let mut used_blocks: u32 = 0;

            // Two bitmaps per group: block bitmap + inode bitmap, each one
            // block in size.
            let mut bitmap = vec![0u8; self.block_size as usize * 2];

            // -- Block bitmap --
            let group_start = group * self.blocks_per_group();
            let group_end = group_start + self.blocks_per_group();

            if group_end <= data_size {
                // Fully allocated group.
                for byte in bitmap[..self.block_size as usize].iter_mut() {
                    *byte = 0xFF;
                }
                used_blocks = self.blocks_per_group();
            } else if group_start < data_size {
                // Partially allocated group.
                let used = data_size - group_start;
                for i in 0..used {
                    bitmap[(i / 8) as usize] |= 1 << (i % 8);
                    used_blocks += 1;
                }
            }

            // Group 0: adjust for unused group descriptor blocks.
            if group == 0 {
                let used_gd_blocks =
                    (total_groups_u32 - 1) / self.groups_per_descriptor_block() + 1;
                // Mark used GD blocks (block 0 = superblock, 1..=used_gd_blocks).
                for i in 0..=used_gd_blocks {
                    bitmap[(i / 8) as usize] |= 1 << (i % 8);
                }
                // Free the reserved-but-unused GD blocks.
                let gd_reserved = self.group_descriptor_blocks();
                for i in (used_gd_blocks + 1)..=gd_reserved {
                    let was_set =
                        (bitmap[(i / 8) as usize] >> (i % 8)) & 1;
                    bitmap[(i / 8) as usize] &= !(1 << (i % 8));
                    if was_set != 0 {
                        used_blocks -= 1;
                    }
                }
            }

            // Mark deleted blocks as free.
            for deleted in &self.deleted_blocks {
                for blk in deleted.start..deleted.end {
                    if blk / self.blocks_per_group() == group {
                        let j = blk % self.blocks_per_group();
                        let was_set =
                            (bitmap[(j / 8) as usize] >> (j % 8)) & 1;
                        bitmap[(j / 8) as usize] &= !(1 << (j % 8));
                        if was_set != 0 {
                            used_blocks -= 1;
                        }
                    }
                }
            }

            // -- Inode bitmap (stored in the second block of the pair) --
            let inode_bitmap_start = self.block_size as usize;
            for i in 0..inodes_per_group {
                let ino = 1 + group * inodes_per_group + i;
                if ino > self.inodes.len() as u32 {
                    continue;
                }
                let inode = &self.inodes[(ino - 1) as usize];
                // Reserved inodes (1..10) are always marked used.
                if ino > 10 && inode.links_count == 0 {
                    continue;
                }
                bitmap[inode_bitmap_start + (i / 8) as usize] |= 1 << (i % 8);
                used_inodes += 1;
                if inode.is_dir() {
                    dirs += 1;
                }
            }
            // Mark remaining inode-bitmap bits (past inodes_per_group) as occupied
            // so the kernel does not try to allocate them.
            for i in (inodes_per_group / 8)..self.block_size {
                bitmap[inode_bitmap_start + i as usize] = 0xFF;
            }

            self.file.write_all(&bitmap)?;

            // -- Build group descriptor --
            let free_blocks = if self.blocks_per_group() >= used_blocks {
                self.blocks_per_group() - used_blocks
            } else {
                0
            };
            let free_inodes = inodes_per_group - used_inodes;
            let block_bitmap_lo = (bitmap_offset + 2 * group) as u32;
            let inode_bitmap_lo = block_bitmap_lo + 1;
            let inode_table_lo =
                (inode_table_offset as u32) + group * inode_table_size_per_group;

            group_descriptors.push(GroupDescriptor {
                block_bitmap_lo,
                inode_bitmap_lo,
                inode_table_lo,
                free_blocks_count_lo: free_blocks as u16,
                free_inodes_count_lo: free_inodes as u16,
                used_dirs_count_lo: dirs as u16,
                flags: 0,
                exclude_bitmap_lo: 0,
                block_bitmap_csum_lo: 0,
                inode_bitmap_csum_lo: 0,
                itable_unused_lo: 0,
                checksum: 0,
            });

            total_used_blocks += used_blocks;
            total_used_inodes += used_inodes;
        }

        // -- Step 5: Extra (empty) block groups beyond data --
        let empty_block_bitmap = {
            let mut bm = vec![0u8; self.blocks_per_group() as usize / 8];
            // Mark inode-table blocks + 2 bitmap blocks as used.
            for i in 0..(inode_table_size_per_group + 2) {
                bm[(i / 8) as usize] |= 1 << (i % 8);
            }
            bm
        };
        let empty_inode_bitmap = {
            let mut bm = vec![0xFFu8; self.blocks_per_group() as usize / 8];
            for i in 0..inodes_per_group as u16 {
                bm[(i / 8) as usize] &= !(1 << (i % 8));
            }
            bm
        };

        for group in block_groups..total_groups_u32 {
            let blocks_in_group = if group == total_groups_u32 - 1 {
                let rem = (self.size / self.block_size as u64) % self.blocks_per_group() as u64;
                if rem == 0 {
                    self.blocks_per_group()
                } else {
                    rem as u32
                }
            } else {
                self.blocks_per_group()
            };

            let bb_offset = group * self.blocks_per_group() + inode_table_size_per_group;
            let ib_offset = bb_offset + 1;
            let it_offset = group as u64 * self.blocks_per_group() as u64;
            let free_blocks_count = blocks_in_group.saturating_sub(inode_table_size_per_group + 2);
            let free_inodes_count = inodes_per_group;

            group_descriptors.push(GroupDescriptor {
                block_bitmap_lo: bb_offset,
                inode_bitmap_lo: ib_offset,
                inode_table_lo: it_offset as u32,
                free_blocks_count_lo: free_blocks_count as u16,
                free_inodes_count_lo: free_inodes_count as u16,
                used_dirs_count_lo: 0,
                flags: 0,
                exclude_bitmap_lo: 0,
                block_bitmap_csum_lo: 0,
                inode_bitmap_csum_lo: 0,
                itable_unused_lo: 0,
                checksum: 0,
            });

            total_used_blocks += inode_table_size_per_group + 2;

            // Write block bitmap + inode bitmap at the right offset.
            self.seek_to_block(bb_offset)?;

            if group == total_groups_u32 - 1
                && blocks_in_group < self.blocks_per_group()
            {
                // Last partial group: mark out-of-range blocks as used.
                let mut partial_bb =
                    vec![0u8; self.blocks_per_group() as usize / 8];
                for i in blocks_in_group..self.blocks_per_group() {
                    partial_bb[(i / 8) as usize] |= 1 << (i % 8);
                }
                for i in 0..(inode_table_size_per_group + 2) {
                    partial_bb[(i / 8) as usize] |= 1 << (i % 8);
                }
                self.file.write_all(&partial_bb)?;
                self.file.write_all(&empty_inode_bitmap)?;
            } else {
                self.file.write_all(&empty_block_bitmap)?;
                self.file.write_all(&empty_inode_bitmap)?;
            }
        }

        // -- Step 6: Write group descriptors at block 1 --
        self.seek_to_block(1)?;
        let mut gd_buf = vec![0u8; GroupDescriptor::SIZE];
        for gd in &group_descriptors {
            gd.write_to(&mut gd_buf);
            self.file.write_all(&gd_buf)?;
        }

        // -- Step 7: Write superblock --
        let computed_inodes = total_groups_u32 as u64 * inodes_per_group as u64;
        let mut blocks_count =
            total_groups_u32 as u64 * self.blocks_per_group() as u64;
        if blocks_count < total_used_blocks as u64 {
            blocks_count = total_used_blocks as u64;
        }
        let total_free_blocks = blocks_count.saturating_sub(total_used_blocks as u64);
        let free_inodes = computed_inodes as u32 - total_used_inodes;

        let mut sb = SuperBlock::default();
        sb.inodes_count = computed_inodes as u32;
        sb.blocks_count_lo = blocks_count as u32;
        sb.blocks_count_hi = (blocks_count >> 32) as u32;
        sb.free_blocks_count_lo = total_free_blocks as u32;
        sb.free_blocks_count_hi = (total_free_blocks >> 32) as u32;
        sb.free_inodes_count = free_inodes;
        sb.first_data_block = 0;
        // log_block_size = log2(block_size / 1024).  E.g. 1024->0, 2048->1, 4096->2.
        let log_bs = (self.block_size / 1024).trailing_zeros();
        sb.log_block_size = log_bs;
        sb.log_cluster_size = log_bs;
        sb.blocks_per_group = self.blocks_per_group();
        sb.clusters_per_group = self.blocks_per_group();
        sb.inodes_per_group = inodes_per_group;
        sb.magic = SUPERBLOCK_MAGIC;
        sb.state = 1; // cleanly unmounted
        sb.errors = 1; // continue on error
        sb.creator_os = 3; // FreeBSD (matches Apple's implementation)
        sb.rev_level = 1; // dynamic inode sizes
        sb.first_ino = FIRST_INODE;
        sb.lpf_ino = LOST_AND_FOUND_INODE;
        sb.inode_size = INODE_SIZE as u16;
        sb.feature_compat = compat::SPARSE_SUPER2 | compat::EXT_ATTR;
        sb.feature_incompat =
            incompat::FILETYPE | incompat::EXTENTS | incompat::FLEX_BG;
        sb.feature_ro_compat =
            ro_compat::LARGE_FILE | ro_compat::HUGE_FILE | ro_compat::EXTRA_ISIZE;
        sb.min_extra_isize = EXTRA_ISIZE;
        sb.want_extra_isize = EXTRA_ISIZE;
        sb.log_groups_per_flex = 31;
        sb.uuid = *self.uuid.unwrap_or_else(Uuid::new_v4).as_bytes();
        if let Some(label) = &self.label {
            let bytes = label.as_bytes();
            sb.volume_name[..bytes.len()].copy_from_slice(bytes);
        }

        // Write: 1024 zero bytes (boot sector) + superblock + 2048 zero bytes.
        self.seek_to_block(0)?;
        self.file.write_all(&[0u8; 1024])?;
        let mut sb_buf = [0u8; SUPERBLOCK_SIZE];
        sb.write_to(&mut sb_buf);
        self.file.write_all(&sb_buf)?;
        self.file.write_all(&[0u8; 2048])?;

        self.file.flush()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// BFS-walk the file tree and write directory entries for every directory
    /// inode.  Updates each directory inode's size and extent tree.
    fn commit_directories(&mut self) -> FormatResult<()> {
        // Collect BFS order: (parent_idx_opt, node_idx).
        let mut queue: std::collections::VecDeque<(Option<usize>, usize)> =
            std::collections::VecDeque::new();
        queue.push_back((None, self.tree.root()));

        let mut bfs_order: Vec<(Option<usize>, usize)> = Vec::new();
        while let Some((parent, node)) = queue.pop_front() {
            bfs_order.push((parent, node));
            // Skip hardlink nodes (no children to descend into).
            if self.tree.node(node).link.is_some() {
                continue;
            }
            let children: Vec<usize> = self.tree.node(node).children.clone();
            for &child in &children {
                queue.push_back((Some(node), child));
            }
        }

        // Commit each directory.
        for (parent_opt, node_idx) in bfs_order {
            let inode_num = self.tree.node(node_idx).inode;
            let inode = &self.inodes[(inode_num - 1) as usize];

            if inode.links_count == 0 {
                continue;
            }
            if self.tree.node(node_idx).link.is_some() {
                continue;
            }
            if !inode.is_dir() {
                continue;
            }

            self.align_to_block()?;
            let start_block = self.current_block();

            let mut left = self.block_size as i32;

            // Write "." entry.
            dir::write_dir_entry(
                &mut self.file,
                ".",
                inode_num,
                self.inodes[(inode_num - 1) as usize].mode,
                None,
                None,
                self.block_size,
                &mut left,
            )?;

            // Write ".." entry.
            let parent_ino = match parent_opt {
                Some(pidx) => self.tree.node(pidx).inode,
                None => inode_num, // root's ".." points to itself
            };
            dir::write_dir_entry(
                &mut self.file,
                "..",
                parent_ino,
                self.inodes[(parent_ino - 1) as usize].mode,
                None,
                None,
                self.block_size,
                &mut left,
            )?;

            // Sort children by inode number for e2fsck compatibility.
            let mut sorted_children: Vec<usize> =
                self.tree.node(node_idx).children.clone();
            sorted_children.sort_by_key(|&ci| self.tree.node(ci).inode);

            for &child_idx in &sorted_children {
                let child_ino = self.tree.node(child_idx).inode;
                let child_name = self.tree.node(child_idx).name.clone();

                // Check that the inode has links (not deleted).
                let effective_ino;
                let effective_mode;
                if let Some(linked_ino) = self.tree.node(child_idx).link {
                    if self.inodes[(linked_ino - 1) as usize].links_count == 0 {
                        continue;
                    }
                    effective_ino = linked_ino;
                    effective_mode = self.inodes[(linked_ino - 1) as usize].mode;
                } else {
                    if self.inodes[(child_ino - 1) as usize].links_count == 0 {
                        continue;
                    }
                    effective_ino = child_ino;
                    effective_mode = self.inodes[(child_ino - 1) as usize].mode;
                }

                let (link_inode, link_mode) =
                    if self.tree.node(child_idx).link.is_some() {
                        (Some(effective_ino), Some(effective_mode))
                    } else {
                        (None, None)
                    };

                dir::write_dir_entry(
                    &mut self.file,
                    &child_name,
                    child_ino,
                    self.inodes[(child_ino.max(1) - 1) as usize].mode,
                    link_inode,
                    link_mode,
                    self.block_size,
                    &mut left,
                )?;
            }

            dir::finish_dir_entry_block(&mut self.file, &mut left, self.block_size)?;

            let end_block = self.current_block();
            let size = (end_block - start_block) as u64 * self.block_size as u64;
            self.inodes[(inode_num - 1) as usize].set_file_size(size);

            // Store block range in the tree node.
            self.tree.node_mut(node_idx).blocks = Some(BlockRange {
                start: start_block,
                end: end_block,
            });

            // Write extent tree for this directory.
            self.align_to_block()?;
            let blocks = BlockRange {
                start: start_block,
                end: end_block,
            };
            let mut cur = self.current_block();
            extent::write_extents(
                &mut self.inodes[(inode_num - 1) as usize],
                blocks,
                self.block_size,
                &mut self.file,
                &mut cur,
            )?;
        }

        Ok(())
    }

    /// Write all inodes sequentially, padding to fill inode tables for all
    /// block groups.
    fn commit_inode_table(
        &mut self,
        block_groups: u32,
        inodes_per_group: u32,
    ) -> FormatResult<u64> {
        self.align_to_block()?;
        let inode_table_offset = self.pos() / self.block_size as u64;

        // Write the actual inodes.
        let mut inode_buf = [0u8; Inode::SIZE];
        for inode in &self.inodes {
            inode.write_to(&mut inode_buf);
            self.file.write_all(&inode_buf)?;
        }

        // Pad the rest of the table with zeros.
        let table_size =
            INODE_SIZE as u64 * block_groups as u64 * inodes_per_group as u64;
        let written = self.inodes.len() as u64 * INODE_SIZE as u64;
        let rest = table_size - written;
        if rest > 0 {
            let zero_block = vec![0u8; self.block_size as usize];
            let full_blocks = rest / self.block_size as u64;
            for _ in 0..full_blocks {
                self.file.write_all(&zero_block)?;
            }
            let remainder = (rest % self.block_size as u64) as usize;
            if remainder > 0 {
                self.file.write_all(&vec![0u8; remainder])?;
            }
        }

        Ok(inode_table_offset)
    }

    /// Find the (block_groups, inodes_per_group) pair that minimizes the
    /// number of block groups needed to hold all inodes and all data blocks.
    fn optimize_block_group_layout(
        &self,
        blocks: u32,
        inodes: u32,
    ) -> (u32, u32) {
        let group_count =
            |blocks: u32, inodes: u32, ipg: u32| -> u32 {
                let inode_blocks_per_group = ipg * INODE_SIZE / self.block_size;
                // 2 blocks reserved for bitmaps.
                let data_blocks_per_group =
                    self.blocks_per_group() - inode_blocks_per_group - 2;
                // Ensure enough groups for all the inodes.
                let min_blocks =
                    (inodes.saturating_sub(1)) / ipg * data_blocks_per_group + 1;
                let effective_blocks = blocks.max(min_blocks);
                (effective_blocks + data_blocks_per_group - 1)
                    / data_blocks_per_group
            };

        let inc = (self.block_size * 512 / INODE_SIZE) as usize;
        let mut best_groups = u32::MAX;
        let mut best_ipg: u32 = inc as u32;

        let mut ipg = inc;
        while ipg <= self.max_inodes_per_group() as usize {
            let g = group_count(blocks, inodes, ipg as u32);
            if g < best_groups {
                best_groups = g;
                best_ipg = ipg as u32;
            }
            ipg += inc;
        }

        (best_groups, best_ipg)
    }
}

// ---------------------------------------------------------------------------
// Free-standing helpers
// ---------------------------------------------------------------------------

/// Return the parent directory of `path`.
///
/// `"/foo/bar"` -> `"/foo"`, `"/foo"` -> `"/"`, `"/"` -> `"/"`.
fn parent_of(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/",
        Some(pos) => &trimmed[..pos],
        None => "/",
    }
}

/// Return the last path component.
///
/// `"/foo/bar"` -> `"bar"`, `"/"` -> `"/"`.
fn basename(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(pos) => &trimmed[pos + 1..],
        None => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parent_of() {
        assert_eq!(parent_of("/"), "/");
        assert_eq!(parent_of("/foo"), "/");
        assert_eq!(parent_of("/foo/bar"), "/foo");
        assert_eq!(parent_of("/a/b/c"), "/a/b");
    }

    #[test]
    fn test_basename() {
        assert_eq!(basename("/"), "/");
        assert_eq!(basename("/foo"), "foo");
        assert_eq!(basename("/foo/bar"), "bar");
        assert_eq!(basename("/a/b/c"), "c");
    }

    #[test]
    fn test_formatter_new_and_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ext4");
        let fmt = Formatter::new(&path, 4096, 256 * 1024).unwrap();
        fmt.close().unwrap();

        // The file should exist and have non-zero size.
        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.len() > 0);
    }

    #[test]
    fn test_create_file_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ext4");
        let mut fmt = Formatter::new(&path, 4096, 1024 * 1024).unwrap();

        // Create a nested directory.
        fmt.create(
            "/etc",
            make_mode(file_mode::S_IFDIR, 0o755),
            None, None, None, None, None, None,
        ).unwrap();

        // Create a regular file with content.
        let data = b"hello world\n";
        let mut cursor = std::io::Cursor::new(&data[..]);
        fmt.create(
            "/etc/motd",
            make_mode(file_mode::S_IFREG, 0o644),
            None, None, Some(&mut cursor), None, None, None,
        ).unwrap();

        // Create a symlink.
        fmt.create(
            "/etc/motd.link",
            make_mode(file_mode::S_IFLNK, 0o777),
            Some("motd"),
            None, None, None, None, None,
        ).unwrap();

        fmt.close().unwrap();
    }

    #[test]
    fn test_hard_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ext4");
        let mut fmt = Formatter::new(&path, 4096, 1024 * 1024).unwrap();

        let data = b"content";
        let mut cursor = std::io::Cursor::new(&data[..]);
        fmt.create(
            "/file",
            make_mode(file_mode::S_IFREG, 0o644),
            None, None, Some(&mut cursor), None, None, None,
        ).unwrap();

        fmt.link("/hardlink", "/file").unwrap();
        fmt.close().unwrap();
    }

    #[test]
    fn test_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ext4");
        let mut fmt = Formatter::new(&path, 4096, 1024 * 1024).unwrap();

        fmt.create(
            "/dir/nested",
            make_mode(file_mode::S_IFDIR, 0o755),
            None, None, None, None, None, None,
        ).unwrap();

        fmt.unlink("/dir", false).unwrap();
        fmt.close().unwrap();
    }

    #[test]
    fn test_mkdir_p_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ext4");
        let mut fmt = Formatter::new(&path, 4096, 1024 * 1024).unwrap();

        // Creating the same directory twice should succeed (mkdir -p semantics).
        fmt.create(
            "/var/log",
            make_mode(file_mode::S_IFDIR, 0o755),
            None, None, None, None, None, None,
        ).unwrap();
        fmt.create(
            "/var/log",
            make_mode(file_mode::S_IFDIR, 0o755),
            None, None, None, None, None, None,
        ).unwrap();

        fmt.close().unwrap();
    }

    #[test]
    fn test_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ext4");
        let mut fmt = Formatter::new(&path, 4096, 4 * 1024 * 1024).unwrap();

        // Create a file larger than one block.
        let data = vec![0xABu8; 8192];
        let mut cursor = std::io::Cursor::new(&data[..]);
        fmt.create(
            "/big",
            make_mode(file_mode::S_IFREG, 0o644),
            None, None, Some(&mut cursor), None, None, None,
        ).unwrap();

        fmt.close().unwrap();
    }
}
