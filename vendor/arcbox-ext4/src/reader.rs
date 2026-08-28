// ext4 filesystem reader.
//
// Opens an ext4 disk image, parses the superblock, and builds an in-memory
// file tree via BFS traversal.  Group descriptors and inodes are cached on
// first access.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::constants::*;
use crate::dir;
use crate::error::{ReadError, ReadResult};
use crate::extent;
use crate::file_tree::{BlockRange, FileTree, FileTreeNode, InodeNumber};
use crate::types::*;

/// Read-only ext4 filesystem reader.
///
/// Parses the superblock, lazily reads group descriptors and inodes, and
/// maintains a [`FileTree`] that mirrors the on-disk directory hierarchy.
pub struct Reader {
    pub(crate) file: File,
    superblock: SuperBlock,
    group_descriptors: HashMap<u32, GroupDescriptor>,
    inodes: HashMap<InodeNumber, Inode>,
    tree: FileTree,
    /// Paths that are hard links to already-seen inodes.
    /// Key: path string (e.g. "usr/bin/link"), Value: target inode number.
    pub hardlinks: HashMap<String, InodeNumber>,
}

impl Reader {
    /// Open an ext4 disk image at `path` and build the in-memory file tree.
    ///
    /// The constructor:
    /// 1. Reads and validates the superblock.
    /// 2. BFS-traverses the directory tree from the root inode.
    /// 3. Records hard links (paths whose inode was already visited).
    /// 4. Reads extent information for every discovered entry.
    pub fn new(path: &Path) -> ReadResult<Self> {
        let mut file = File::open(path).map_err(|_| ReadError::NotFound(path.to_path_buf()))?;

        // -- Parse superblock --------------------------------------------------
        file.seek(SeekFrom::Start(SUPERBLOCK_OFFSET))?;
        let mut sb_buf = [0u8; SUPERBLOCK_SIZE];
        file.read_exact(&mut sb_buf).map_err(|_| {
            ReadError::CouldNotReadSuperBlock(path.to_path_buf(), SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE)
        })?;
        let superblock = SuperBlock::read_from(&sb_buf);

        if superblock.magic != SUPERBLOCK_MAGIC {
            return Err(ReadError::InvalidSuperBlock);
        }

        let mut reader = Self {
            file,
            superblock,
            group_descriptors: HashMap::new(),
            inodes: HashMap::new(),
            tree: FileTree::new(ROOT_INODE, "."),
            hardlinks: HashMap::new(),
        };

        // -- BFS traversal to build the file tree ------------------------------
        // Each work item is (tree node index, inode number).
        let mut queue: Vec<(usize, InodeNumber)> = vec![(reader.tree.root(), ROOT_INODE)];

        while let Some((parent_idx, inode_num)) = queue.pop() {
            let children = reader.get_dir_entries(inode_num)?;

            for (name, child_ino) in children {
                // Skip the "." and ".." pseudo-entries.
                if name == "." || name == ".." {
                    continue;
                }

                // If we have already seen this inode, this entry is a hard link.
                if reader.inodes.contains_key(&child_ino) {
                    let parent_path = reader.tree.node_path(parent_idx);
                    let full = parent_path.join(&name);
                    // Store as a forward-slash path without leading "/".
                    let key = full
                        .to_string_lossy()
                        .trim_start_matches('/')
                        .trim_start_matches("./")
                        .to_string();
                    reader.hardlinks.insert(key, child_ino);
                    continue;
                }

                // Read extents for this entry.
                let blocks = extent::parse_extents(
                    &reader.get_inode(child_ino)?,
                    reader.block_size(),
                    &mut reader.file,
                )?;

                let mut node = FileTreeNode {
                    inode: child_ino,
                    name: name.clone(),
                    children: Vec::new(),
                    parent: None,
                    blocks: None,
                    additional_blocks: Vec::new(),
                    link: None,
                };

                // Map extent ranges to BlockRange values.
                if let Some(first) = blocks.first() {
                    node.blocks = Some(BlockRange {
                        start: first.0,
                        end: first.1,
                    });
                }
                for range in blocks.iter().skip(1) {
                    node.additional_blocks.push(BlockRange {
                        start: range.0,
                        end: range.1,
                    });
                }

                let child_idx = reader.tree.add_child(parent_idx, node);

                // If the child is a directory, enqueue it for further traversal.
                let child_inode = reader.get_inode(child_ino)?;
                if child_inode.is_dir() {
                    queue.push((child_idx, child_ino));
                }
            }
        }

        Ok(reader)
    }

    // -- Public accessors ------------------------------------------------------

    /// Borrow the parsed superblock.
    pub fn superblock(&self) -> &SuperBlock {
        &self.superblock
    }

    /// Borrow the in-memory file tree.
    pub fn tree(&self) -> &FileTree {
        &self.tree
    }

    // -- Block size helpers ----------------------------------------------------

    /// Filesystem block size in bytes, derived from `log_block_size`.
    pub(crate) fn block_size(&self) -> u64 {
        1024 * (1u64 << self.superblock.log_block_size)
    }

    /// On-disk group descriptor size.  When the 64-bit feature flag is set, the
    /// superblock's `desc_size` is used; otherwise the base 32-byte descriptor.
    fn group_descriptor_size(&self) -> u16 {
        if self.superblock.feature_incompat & incompat::BIT64 != 0 {
            self.superblock.desc_size
        } else {
            GroupDescriptor::SIZE as u16
        }
    }

    // -- Low-level I/O ---------------------------------------------------------

    /// Read a group descriptor from disk (uncached).
    fn read_group_descriptor(&mut self, number: u32) -> ReadResult<GroupDescriptor> {
        let bs = self.block_size();
        let offset = bs + number as u64 * self.group_descriptor_size() as u64;
        self.file.seek(SeekFrom::Start(offset))?;

        let mut buf = [0u8; 64]; // Enough for both 32- and 64-byte descriptors.
        let read_len = GroupDescriptor::SIZE;
        self.file
            .read_exact(&mut buf[..read_len])
            .map_err(|_| ReadError::CouldNotReadGroup(number))?;

        Ok(GroupDescriptor::read_from(&buf))
    }

    /// Get a group descriptor, reading from disk on first access.
    pub fn get_group_descriptor(&mut self, number: u32) -> ReadResult<GroupDescriptor> {
        if let Some(gd) = self.group_descriptors.get(&number) {
            return Ok(gd.clone());
        }
        let gd = self.read_group_descriptor(number)?;
        self.group_descriptors.insert(number, gd.clone());
        Ok(gd)
    }

    /// Read an inode from disk (uncached).
    fn read_inode(&mut self, number: u32) -> ReadResult<Inode> {
        let group = (number - 1) / self.superblock.inodes_per_group;
        let index_in_group = ((number - 1) % self.superblock.inodes_per_group) as u64;
        let gd = self.get_group_descriptor(group)?;
        let table_start = gd.inode_table_lo as u64 * self.block_size();
        let inode_offset = table_start + index_in_group * self.superblock.inode_size as u64;

        self.file.seek(SeekFrom::Start(inode_offset))?;

        let mut buf = [0u8; INODE_SIZE as usize];
        self.file
            .read_exact(&mut buf)
            .map_err(|_| ReadError::CouldNotReadInode(number))?;

        Ok(Inode::read_from(&buf))
    }

    /// Get an inode, reading from disk on first access.
    pub fn get_inode(&mut self, number: u32) -> ReadResult<Inode> {
        if let Some(inode) = self.inodes.get(&number) {
            return Ok(inode.clone());
        }
        let inode = self.read_inode(number)?;
        self.inodes.insert(number, inode.clone());
        Ok(inode)
    }

    /// Parse directory entries for the given directory inode.
    ///
    /// Reads the extent tree to find the directory's data blocks, then parses
    /// each block with [`dir::parse_dir_entries`].  Results are sorted
    /// alphabetically by name for deterministic traversal.
    fn get_dir_entries(&mut self, inode_number: InodeNumber) -> ReadResult<Vec<(String, InodeNumber)>> {
        let inode = self.get_inode(inode_number)?;
        let extents = extent::parse_extents(&inode, self.block_size(), &mut self.file)?;
        let bs = self.block_size() as usize;

        let mut entries = Vec::new();

        for (phys_start, phys_end) in &extents {
            self.seek_to_block(*phys_start)?;
            let num_blocks = phys_end - phys_start;
            for i in 0..num_blocks {
                let mut block_buf = vec![0u8; bs];
                self.file
                    .read_exact(&mut block_buf)
                    .map_err(|_| ReadError::CouldNotReadBlock(phys_start + i))?;
                let block_entries = dir::parse_dir_entries(&block_buf);
                entries.extend(block_entries);
            }
        }

        // Sort alphabetically for deterministic ordering.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    /// List the children of a directory inode (public wrapper around
    /// [`get_dir_entries`]).
    pub fn children_of(&mut self, number: InodeNumber) -> ReadResult<Vec<(String, InodeNumber)>> {
        self.get_dir_entries(number)
    }

    /// Seek the underlying file handle to the start of a physical block.
    fn seek_to_block(&mut self, block: u32) -> ReadResult<()> {
        self.file
            .seek(SeekFrom::Start(block as u64 * self.block_size()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Formatter;

    /// Helper: create a formatter backed by a temp file, returning the reader
    /// after closing the formatter.
    fn make_reader_with<F>(setup: F) -> Reader
    where
        F: FnOnce(&mut Formatter),
    {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut fmt = Formatter::new(tmp.path(), 4096, 256 * 1024).unwrap();
        setup(&mut fmt);
        fmt.close().unwrap();
        Reader::new(tmp.path()).unwrap()
    }

    #[test]
    fn test_superblock_fields_after_roundtrip() {
        let reader = make_reader_with(|_fmt| {
            // Empty filesystem -- just root + lost+found.
        });

        let sb = reader.superblock();

        // The magic number must be the ext4 signature.
        assert_eq!(sb.magic, SUPERBLOCK_MAGIC);

        // log_block_size=2 means 1024 * (1 << 2) = 4096 bytes per block.
        assert_eq!(sb.log_block_size, 2);
        assert_eq!(reader.block_size(), 4096);

        // The first non-reserved inode must be FIRST_INODE (11).
        assert_eq!(sb.first_ino, FIRST_INODE);

        // Inode size should be 256.
        assert_eq!(sb.inode_size, INODE_SIZE as u16);

        // The extents feature flag must be set.
        assert_ne!(sb.feature_incompat & incompat::EXTENTS, 0);
    }

    #[test]
    fn test_children_of_root_inode() {
        let mut reader = make_reader_with(|fmt| {
            // Create a few entries in the root directory.
            fmt.create(
                "/alpha",
                make_mode(file_mode::S_IFREG, 0o644),
                None,
                None,
                Some(&mut "a".as_bytes()),
                None,
                None,
                None,
            )
            .unwrap();
            fmt.create(
                "/beta",
                make_mode(file_mode::S_IFDIR, 0o755),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            fmt.create(
                "/gamma.txt",
                make_mode(file_mode::S_IFREG, 0o600),
                None,
                None,
                Some(&mut "g".as_bytes()),
                None,
                None,
                None,
            )
            .unwrap();
        });

        let children = reader.children_of(ROOT_INODE).unwrap();

        // Filter out "." and ".." to get real entries.
        let names: Vec<&str> = children
            .iter()
            .filter(|(n, _)| n != "." && n != "..")
            .map(|(n, _)| n.as_str())
            .collect();

        // lost+found is always created by the formatter, plus our three entries.
        assert!(names.contains(&"lost+found"));
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(names.contains(&"gamma.txt"));
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn test_get_inode_root() {
        let mut reader = make_reader_with(|_fmt| {});

        // The root inode should be a directory.
        let root_inode = reader.get_inode(ROOT_INODE).unwrap();
        assert!(root_inode.is_dir());
        assert!(!root_inode.is_reg());
        assert!(!root_inode.is_link());
    }

    #[test]
    fn test_block_size_calculation() {
        // 4096-byte blocks: log_block_size should be 2.
        let reader = make_reader_with(|_fmt| {});
        assert_eq!(reader.block_size(), 4096);
    }
}
