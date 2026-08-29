// Directory entry writing (Formatter) and parsing (Reader).
//
// On-disk directory entries are variable-length records packed into blocks.
// Each entry has an 8-byte header (DirectoryEntry) followed by the name bytes
// and padding to a 4-byte boundary.

use crate::constants::*;
use crate::types::*;
use std::io::{self, Write};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Round `n` up to the next multiple of `align`.
#[inline]
fn align_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

/// Determine the directory-entry file type from an inode mode.
fn dir_file_type(mode: u16) -> u8 {
    FileType::from_mode(mode) as u8
}

// ---------------------------------------------------------------------------
// Write path (Formatter)
// ---------------------------------------------------------------------------

/// Write a single directory entry.
///
/// `left` tracks the number of remaining bytes in the current directory block.
/// If there is not enough room for both this entry and a trailing terminator
/// (12 bytes minimum), the current block is finished first with a zero-inode
/// terminator before writing this entry at the start of a new block.
///
/// When `link_inode` is `Some`, the entry's inode number is taken from
/// `link_inode` and the file type is derived from `link_mode` (the target
/// inode's mode).  Otherwise, `inode` and `mode` are used directly.
pub fn write_dir_entry<W: Write>(
    writer: &mut W,
    name: &str,
    inode: u32,
    mode: u16,
    link_inode: Option<u32>,
    link_mode: Option<u16>,
    block_size: u32,
    left: &mut i32,
) -> io::Result<()> {
    let name_bytes = name.as_bytes();
    let entry_size = align_up(DirectoryEntry::SIZE + name_bytes.len(), 4);

    // Minimum trailing entry is 12 bytes (header 8 + 4 for alignment).
    let min_trailing = 12;

    // If the current block does not have room for this entry plus a trailing
    // terminator, finish the block first.
    if (*left as usize) < entry_size + min_trailing {
        finish_dir_entry_block(writer, left, block_size)?;
    }

    // Resolve inode number and file type for hard links.
    let actual_inode = link_inode.unwrap_or(inode);
    let actual_mode = link_mode.unwrap_or(mode);

    let entry = DirectoryEntry {
        inode: actual_inode,
        rec_len: entry_size as u16,
        name_len: name_bytes.len() as u8,
        file_type: dir_file_type(actual_mode),
    };

    let mut header_buf = [0u8; DirectoryEntry::SIZE];
    entry.write_to(&mut header_buf);
    writer.write_all(&header_buf)?;

    // Write name bytes.
    writer.write_all(name_bytes)?;

    // Write padding zeros to align to 4 bytes.
    let padding = entry_size - DirectoryEntry::SIZE - name_bytes.len();
    if padding > 0 {
        let zeros = [0u8; 4];
        writer.write_all(&zeros[..padding])?;
    }

    *left -= entry_size as i32;

    Ok(())
}

/// Finish the current directory entry block by writing a zero-inode terminator
/// entry that consumes all remaining space.
///
/// If `left` is already <= 0 (block boundary was exactly reached), reset
/// `left` to `block_size` and return without writing.
pub fn finish_dir_entry_block<W: Write>(
    writer: &mut W,
    left: &mut i32,
    block_size: u32,
) -> io::Result<()> {
    if *left <= 0 {
        *left = block_size as i32;
        return Ok(());
    }

    let remaining = *left as usize;

    // Write a terminator entry: inode=0, rec_len=remaining, name_len=0, file_type=0.
    let term = DirectoryEntry {
        inode: 0,
        rec_len: remaining as u16,
        name_len: 0,
        file_type: 0,
    };

    let mut header_buf = [0u8; DirectoryEntry::SIZE];
    term.write_to(&mut header_buf);
    writer.write_all(&header_buf)?;

    // Fill remaining bytes with zeros.
    let fill = remaining - DirectoryEntry::SIZE;
    if fill > 0 {
        let zeros = vec![0u8; fill];
        writer.write_all(&zeros)?;
    }

    *left = block_size as i32;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read path (Reader)
// ---------------------------------------------------------------------------

/// Parse directory entries from a block of raw data.
///
/// Returns a vector of `(name, inode_number)` pairs.  Deleted entries
/// (inode == 0) and entries with zero-length names are skipped.
pub fn parse_dir_entries(data: &[u8]) -> Vec<(String, u32)> {
    let mut entries = Vec::new();
    let mut offset = 0;

    while offset + DirectoryEntry::SIZE <= data.len() {
        let entry = DirectoryEntry::read_from(&data[offset..]);

        // rec_len of 0 means we'd loop forever.
        if entry.rec_len == 0 {
            break;
        }

        if entry.inode != 0 && entry.name_len > 0 {
            let name_start = offset + DirectoryEntry::SIZE;
            let name_end = name_start + entry.name_len as usize;

            if name_end <= data.len() {
                let name = String::from_utf8_lossy(&data[name_start..name_end]).into_owned();
                entries.push((name, entry.inode));
            }
        }

        offset += entry.rec_len as usize;
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_up() {
        assert_eq!(align_up(0, 4), 0);
        assert_eq!(align_up(1, 4), 4);
        assert_eq!(align_up(4, 4), 4);
        assert_eq!(align_up(5, 4), 8);
        assert_eq!(align_up(8, 4), 8);
        assert_eq!(align_up(9, 4), 12);
    }

    #[test]
    fn test_write_and_parse_dir_entries() {
        let block_size = 4096u32;
        let mut buf = Vec::new();
        let mut left = block_size as i32;

        // Write "." entry.
        write_dir_entry(
            &mut buf, ".", 2, file_mode::S_IFDIR | 0o755,
            None, None, block_size, &mut left,
        ).unwrap();

        // Write ".." entry.
        write_dir_entry(
            &mut buf, "..", 2, file_mode::S_IFDIR | 0o755,
            None, None, block_size, &mut left,
        ).unwrap();

        // Write a regular file entry.
        write_dir_entry(
            &mut buf, "hello.txt", 11, file_mode::S_IFREG | 0o644,
            None, None, block_size, &mut left,
        ).unwrap();

        // Finish the block.
        finish_dir_entry_block(&mut buf, &mut left, block_size).unwrap();

        assert_eq!(buf.len(), block_size as usize);
        assert_eq!(left, block_size as i32);

        // Parse back.
        let entries = parse_dir_entries(&buf);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], (".".to_string(), 2));
        assert_eq!(entries[1], ("..".to_string(), 2));
        assert_eq!(entries[2], ("hello.txt".to_string(), 11));
    }

    #[test]
    fn test_finish_dir_entry_block_at_boundary() {
        let block_size = 4096u32;
        let mut buf = Vec::new();
        let mut left = 0i32;

        // Already at a block boundary -- should just reset `left`.
        finish_dir_entry_block(&mut buf, &mut left, block_size).unwrap();
        assert_eq!(buf.len(), 0);
        assert_eq!(left, block_size as i32);
    }

    #[test]
    fn test_hard_link_entry() {
        let block_size = 4096u32;
        let mut buf = Vec::new();
        let mut left = block_size as i32;

        // Write a hard link entry: display name "link.txt", but pointing to inode 42
        // which is a regular file.
        write_dir_entry(
            &mut buf, "link.txt", 99, file_mode::S_IFREG | 0o644,
            Some(42), Some(file_mode::S_IFREG | 0o644),
            block_size, &mut left,
        ).unwrap();

        finish_dir_entry_block(&mut buf, &mut left, block_size).unwrap();

        let entries = parse_dir_entries(&buf);
        assert_eq!(entries.len(), 1);
        // The inode number should be the link target (42), not the original (99).
        assert_eq!(entries[0], ("link.txt".to_string(), 42));
    }

    #[test]
    fn test_parse_empty_block() {
        let data = vec![0u8; 4096];
        let entries = parse_dir_entries(&data);
        // All-zero block has rec_len=0 in the first entry, so parsing stops immediately.
        assert!(entries.is_empty());
    }
}
