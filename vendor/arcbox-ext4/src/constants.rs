// ── Magic numbers ──

pub const SUPERBLOCK_MAGIC: u16 = 0xEF53;
pub const EXTENT_HEADER_MAGIC: u16 = 0xF30A;
pub const XATTR_HEADER_MAGIC: u32 = 0xEA02_0000;

// ── Reserved inode numbers ──

pub const DEFECTIVE_BLOCK_INODE: u32 = 1;
pub const ROOT_INODE: u32 = 2;
pub const FIRST_INODE: u32 = 11;
pub const LOST_AND_FOUND_INODE: u32 = 11;

// ── Inode sizing ──

/// Bytes used by inode metadata (before inline xattrs).
pub const INODE_ACTUAL_SIZE: u32 = 160;
/// Bytes available for inline extended attributes.
pub const INODE_EXTRA_SIZE: u32 = 96;
/// Total on-disk inode size (metadata + inline xattrs).
pub const INODE_SIZE: u32 = 256;
/// `extra_isize` field value (INODE_ACTUAL_SIZE - 128).
pub const EXTRA_ISIZE: u16 = (INODE_ACTUAL_SIZE - 128) as u16;
/// Size of the xattr header within an inode's inline area.
pub const XATTR_INODE_HEADER_SIZE: u32 = 4;
/// Size of the xattr header for a separate xattr block.
pub const XATTR_BLOCK_HEADER_SIZE: u32 = 32;

// ── Limits ──

pub const MAX_LINKS: u32 = 65000;
pub const MAX_BLOCKS_PER_EXTENT: u32 = 0x8000;
pub const MAX_FILE_SIZE: u64 = 128 * 1024 * 1024 * 1024; // 128 GiB
pub const SUPERBLOCK_OFFSET: u64 = 1024;

// ── Inode block field size ──

/// The `block` field in the inode is 60 bytes, used for extent tree or inline symlink.
pub const INODE_BLOCK_SIZE: usize = 60;

// ── Compatible feature flags ──

pub mod compat {
    pub const DIR_PREALLOC: u32 = 0x1;
    pub const IMAGIC_INODES: u32 = 0x2;
    pub const HAS_JOURNAL: u32 = 0x4;
    pub const EXT_ATTR: u32 = 0x8;
    pub const RESIZE_INODE: u32 = 0x10;
    pub const DIR_INDEX: u32 = 0x20;
    pub const LAZY_BG: u32 = 0x40;
    pub const EXCLUDE_INODE: u32 = 0x80;
    pub const EXCLUDE_BITMAP: u32 = 0x100;
    pub const SPARSE_SUPER2: u32 = 0x200;
}

// ── Incompatible feature flags ──

pub mod incompat {
    pub const COMPRESSION: u32 = 0x1;
    pub const FILETYPE: u32 = 0x2;
    pub const RECOVER: u32 = 0x4;
    pub const JOURNAL_DEV: u32 = 0x8;
    pub const META_BG: u32 = 0x10;
    pub const EXTENTS: u32 = 0x40;
    pub const BIT64: u32 = 0x80;
    pub const MMP: u32 = 0x100;
    pub const FLEX_BG: u32 = 0x200;
    pub const EA_INODE: u32 = 0x400;
    pub const DIRDATA: u32 = 0x1000;
    pub const CSUM_SEED: u32 = 0x2000;
    pub const LARGEDIR: u32 = 0x4000;
    pub const INLINE_DATA: u32 = 0x8000;
    pub const ENCRYPT: u32 = 0x10000;
}

// ── Read-only compatible feature flags ──

pub mod ro_compat {
    pub const SPARSE_SUPER: u32 = 0x1;
    pub const LARGE_FILE: u32 = 0x2;
    pub const BTREE_DIR: u32 = 0x4;
    pub const HUGE_FILE: u32 = 0x8;
    pub const GDT_CSUM: u32 = 0x10;
    pub const DIR_NLINK: u32 = 0x20;
    pub const EXTRA_ISIZE: u32 = 0x40;
    pub const HAS_SNAPSHOT: u32 = 0x80;
    pub const QUOTA: u32 = 0x100;
    pub const BIGALLOC: u32 = 0x200;
    pub const METADATA_CSUM: u32 = 0x400;
    pub const REPLICA: u32 = 0x800;
    pub const READONLY: u32 = 0x1000;
    pub const PROJECT: u32 = 0x2000;
}

// ── Block group flags ──

pub mod bg_flags {
    pub const INODE_UNINIT: u16 = 0x1;
    pub const BLOCK_UNINIT: u16 = 0x2;
    pub const INODE_ZEROED: u16 = 0x4;
}

// ── Inode flags ──

pub mod inode_flags {
    pub const SECRM: u32 = 0x1;
    pub const UNRM: u32 = 0x2;
    pub const COMPRESSED: u32 = 0x4;
    pub const SYNC: u32 = 0x8;
    pub const IMMUTABLE: u32 = 0x10;
    pub const APPEND: u32 = 0x20;
    pub const NODUMP: u32 = 0x40;
    pub const NOATIME: u32 = 0x80;
    pub const DIRTY_COMPRESSED: u32 = 0x100;
    pub const COMPRESSED_CLUSTERS: u32 = 0x200;
    pub const NO_COMPRESS: u32 = 0x400;
    pub const ENCRYPTED: u32 = 0x800;
    pub const HASHED_INDEX: u32 = 0x1000;
    pub const MAGIC: u32 = 0x2000;
    pub const JOURNAL_DATA: u32 = 0x4000;
    pub const NO_TAIL: u32 = 0x8000;
    pub const DIR_SYNC: u32 = 0x10000;
    pub const TOP_DIR: u32 = 0x20000;
    pub const HUGE_FILE: u32 = 0x40000;
    pub const EXTENTS: u32 = 0x80000;
    pub const EA_INODE: u32 = 0x200000;
    pub const EOF_BLOCKS: u32 = 0x400000;
    pub const SNAPFILE: u32 = 0x0100_0000;
    pub const SNAPFILE_DELETED: u32 = 0x0400_0000;
    pub const SNAPFILE_SHRUNK: u32 = 0x0800_0000;
    pub const INLINE_DATA: u32 = 0x1000_0000;
    pub const PROJECT_ID_INHERIT: u32 = 0x2000_0000;
    pub const RESERVED: u32 = 0x8000_0000;
}

// ── File mode flags ──

pub mod file_mode {
    pub const S_IXOTH: u16 = 0x1;
    pub const S_IWOTH: u16 = 0x2;
    pub const S_IROTH: u16 = 0x4;
    pub const S_IXGRP: u16 = 0x8;
    pub const S_IWGRP: u16 = 0x10;
    pub const S_IRGRP: u16 = 0x20;
    pub const S_IXUSR: u16 = 0x40;
    pub const S_IWUSR: u16 = 0x80;
    pub const S_IRUSR: u16 = 0x100;
    pub const S_ISVTX: u16 = 0x200;
    pub const S_ISGID: u16 = 0x400;
    pub const S_ISUID: u16 = 0x800;
    pub const S_IFIFO: u16 = 0x1000;
    pub const S_IFCHR: u16 = 0x2000;
    pub const S_IFDIR: u16 = 0x4000;
    pub const S_IFBLK: u16 = 0x6000;
    pub const S_IFREG: u16 = 0x8000;
    pub const S_IFLNK: u16 = 0xA000;
    pub const S_IFSOCK: u16 = 0xC000;
    pub const TYPE_MASK: u16 = 0xF000;
}

// ── Directory entry file types ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileType {
    Unknown = 0,
    Regular = 1,
    Directory = 2,
    Character = 3,
    Block = 4,
    Fifo = 5,
    Socket = 6,
    SymbolicLink = 7,
}

impl FileType {
    /// Derive the directory-entry file type from an inode mode.
    pub fn from_mode(mode: u16) -> Self {
        match mode & file_mode::TYPE_MASK {
            file_mode::S_IFREG => FileType::Regular,
            file_mode::S_IFDIR => FileType::Directory,
            file_mode::S_IFCHR => FileType::Character,
            file_mode::S_IFBLK => FileType::Block,
            file_mode::S_IFIFO => FileType::Fifo,
            file_mode::S_IFSOCK => FileType::Socket,
            file_mode::S_IFLNK => FileType::SymbolicLink,
            _ => FileType::Unknown,
        }
    }
}

// ── Mode helpers ──

/// Compose a full inode mode from a type flag and permission bits.
#[inline]
pub const fn make_mode(file_type: u16, perm: u16) -> u16 {
    file_type | perm
}

/// Check whether a mode represents a directory.
#[inline]
pub const fn is_dir(mode: u16) -> bool {
    mode & file_mode::TYPE_MASK == file_mode::S_IFDIR
}

/// Check whether a mode represents a regular file.
#[inline]
pub const fn is_reg(mode: u16) -> bool {
    mode & file_mode::TYPE_MASK == file_mode::S_IFREG
}

/// Check whether a mode represents a symbolic link.
#[inline]
pub const fn is_link(mode: u16) -> bool {
    mode & file_mode::TYPE_MASK == file_mode::S_IFLNK
}
