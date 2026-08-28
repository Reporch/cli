use std::path::PathBuf;

/// Errors that can occur when reading an ext4 filesystem.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("file at path {0} not found")]
    NotFound(PathBuf),
    #[error("could not read {2} bytes of superblock from {0} at offset {1}")]
    CouldNotReadSuperBlock(PathBuf, u64, usize),
    #[error("not a valid ext4 superblock")]
    InvalidSuperBlock,
    #[error("deep extents (depth > 1) are not supported")]
    DeepExtentsUnsupported,
    #[error("extents invalid or corrupted")]
    InvalidExtents,
    #[error("invalid extended attribute entry")]
    InvalidXattrEntry,
    #[error("could not read block {0}")]
    CouldNotReadBlock(u32),
    #[error("could not read inode {0}")]
    CouldNotReadInode(u32),
    #[error("could not read group descriptor {0}")]
    CouldNotReadGroup(u32),
    #[error("no such file or directory: {0}")]
    PathNotFound(String),
    #[error("not a regular file: {0}")]
    NotAFile(String),
    #[error("is a directory: {0}")]
    IsDirectory(String),
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("symlink loop while resolving: {0}")]
    SymlinkLoop(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Errors that can occur when formatting an ext4 filesystem.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("{0} is not a directory")]
    NotDirectory(PathBuf),
    #[error("{0} is not a file")]
    NotFile(PathBuf),
    #[error("{0} not found")]
    NotFound(PathBuf),
    #[error("{0} already exists")]
    AlreadyExists(PathBuf),
    #[error("file type not supported")]
    UnsupportedFiletype,
    #[error("maximum links exceeded for path: {0}")]
    MaximumLinksExceeded(PathBuf),
    #[error("{0} exceeds max file size (128 GiB)")]
    FileTooBig(u64),
    #[error("'{0}' is an invalid link")]
    InvalidLink(PathBuf),
    #[error("'{0}' is an invalid name")]
    InvalidName(String),
    #[error("not enough space for trailing directory entry")]
    NoSpaceForTrailingDirEntry,
    #[error("not enough space for group descriptor blocks")]
    InsufficientSpaceForGroupDescriptorBlocks,
    #[error("cannot create hard links to directory target: {0}")]
    CannotHardlinkDirectory(PathBuf),
    #[error("unsupported block size {0} (only 4096 is supported)")]
    UnsupportedBlockSize(u32),
    #[error("invalid volume label: {0}")]
    InvalidLabel(String),
    #[error("cannot truncate file: {0}")]
    CannotTruncateFile(PathBuf),
    #[error("cannot create sparse file at {0}")]
    CannotCreateSparseFile(PathBuf),
    #[error("cannot resize fs to {0} bytes")]
    CannotResizeFs(u64),
    #[error("cannot fit xattr for inode {0}")]
    XattrInsufficientSpace(u32),
    #[error("malformed extended attribute buffer")]
    MalformedXattrBuffer,
    #[error("cannot convert string '{0}' to ASCII")]
    InvalidAsciiString(String),
    #[error("circular hard links found")]
    CircularLinks,
    #[error("invalid path encoding for '{0}', must be ascii or utf8")]
    InvalidPathEncoding(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type ReadResult<T> = std::result::Result<T, ReadError>;
pub type FormatResult<T> = std::result::Result<T, FormatError>;
