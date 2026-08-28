// Extent tree creation and parsing.
//
// Write path: build the extent tree for an inode's data blocks and serialize
// it into the inode's `block` field (and overflow blocks when needed).
//
// Read path: parse an extent tree from an inode's `block` field, returning
// physical block ranges.

use crate::constants::*;
use crate::file_tree::BlockRange;
use crate::types::*;
use std::io::{self, Read, Seek, SeekFrom, Write};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Round `n` up to the next multiple of `align`.
#[inline]
fn div_ceil(n: u32, d: u32) -> u32 {
    (n + d - 1) / d
}

/// Build extent leaves that cover `num_blocks` starting at physical block
/// `start`, using at most `MAX_BLOCKS_PER_EXTENT` blocks per extent.
/// `offset` is the starting logical block number.
fn fill_extent_leaves(start: u32, num_blocks: u32, num_extents: u32, offset: u32) -> Vec<ExtentLeaf> {
    let mut leaves = Vec::with_capacity(num_extents as usize);
    let mut remaining = num_blocks;
    let mut phys = start;
    let mut logical = offset;

    for _ in 0..num_extents {
        let len = remaining.min(MAX_BLOCKS_PER_EXTENT);
        leaves.push(ExtentLeaf {
            block: logical,
            len: len as u16,
            start_hi: 0,
            start_lo: phys,
        });
        phys += len;
        logical += len;
        remaining -= len;
    }

    leaves
}

// ---------------------------------------------------------------------------
// Write path (Formatter)
// ---------------------------------------------------------------------------

/// Write the extent tree for an inode's data blocks into the inode's `block`
/// field.
///
/// If the file spans more than 4 extents, overflow extent blocks are written
/// to `writer` and the inode's block field contains index entries pointing to
/// them.
///
/// Updates `inode.block`, `inode.blocks_lo`, and `inode.flags` in place.
pub fn write_extents<W: Write + Seek>(
    inode: &mut Inode,
    blocks: BlockRange,
    block_size: u32,
    writer: &mut W,
    current_block: &mut u32,
) -> io::Result<()> {
    let data_blocks = blocks.end - blocks.start;
    if data_blocks == 0 {
        return Ok(());
    }

    let num_extents = div_ceil(data_blocks, MAX_BLOCKS_PER_EXTENT);

    // The inode's block field is 60 bytes = header(12) + 4 * entry(12).
    // So we can fit up to 4 extents inline.
    let max_inline = 4u32;

    if num_extents <= max_inline {
        // Case: 1-4 extents -- everything fits in the inode's block field.
        write_inline_extents(inode, blocks.start, data_blocks, num_extents, block_size);
    } else {
        // Case: 5+ extents -- depth-1 tree with index entries in the inode
        // and leaf blocks written to the output.
        write_indexed_extents(inode, blocks.start, data_blocks, num_extents, block_size, writer, current_block)?;
    }

    Ok(())
}

/// Write extent tree inline (depth 0): header + leaves directly in inode.block.
fn write_inline_extents(
    inode: &mut Inode,
    start: u32,
    data_blocks: u32,
    num_extents: u32,
    block_size: u32,
) {
    let leaves = fill_extent_leaves(start, data_blocks, num_extents, 0);

    let mut buf = [0u8; INODE_BLOCK_SIZE];

    // Write extent header.
    let header = ExtentHeader {
        magic: EXTENT_HEADER_MAGIC,
        entries: num_extents as u16,
        max: 4,
        depth: 0,
        generation: 0,
    };
    header.write_to(&mut buf[..ExtentHeader::SIZE]);

    // Write leaf entries.
    for (i, leaf) in leaves.iter().enumerate() {
        let off = ExtentHeader::SIZE + i * ExtentLeaf::SIZE;
        leaf.write_to(&mut buf[off..off + ExtentLeaf::SIZE]);
    }

    inode.block = buf;

    // When the HUGE_FILE inode flag is set (and the ro_compat HUGE_FILE
    // feature is enabled on the filesystem), blocks_lo counts in units of
    // filesystem blocks.  Otherwise it counts 512-byte sectors.
    if inode.flags & inode_flags::HUGE_FILE != 0 {
        inode.blocks_lo = data_blocks;
    } else {
        let sectors = (data_blocks as u64) * (block_size as u64 / 512);
        inode.blocks_lo = sectors as u32;
    }
    inode.flags |= inode_flags::EXTENTS;
}

/// Write extent tree with depth 1: index entries in inode.block, leaf blocks
/// written to the output stream.
fn write_indexed_extents<W: Write + Seek>(
    inode: &mut Inode,
    start: u32,
    data_blocks: u32,
    num_extents: u32,
    block_size: u32,
    writer: &mut W,
    current_block: &mut u32,
) -> io::Result<()> {
    // How many leaf entries fit in one extent block?
    // Block layout: header(12) + N * leaf(12) + tail(4).
    // N = (block_size - 12 - 4) / 12
    let leaves_per_block = (block_size as usize - ExtentHeader::SIZE - ExtentTail::SIZE) / ExtentLeaf::SIZE;

    // How many index blocks do we need?
    let num_index_blocks = div_ceil(num_extents, leaves_per_block as u32);

    // The inode can hold up to 4 index entries (same 60-byte limit).
    debug_assert!(num_index_blocks <= 4, "files requiring >4 index blocks (depth>1) are not supported");

    let all_leaves = fill_extent_leaves(start, data_blocks, num_extents, 0);

    // Allocate block numbers for the index (leaf-block) nodes.
    // They are written sequentially starting at *current_block.
    let index_block_start = *current_block;

    // Build the index entries.
    let mut indices = Vec::with_capacity(num_index_blocks as usize);
    for i in 0..num_index_blocks as usize {
        let first_leaf_in_block = i * leaves_per_block;
        let logical_block = all_leaves[first_leaf_in_block].block;
        indices.push(ExtentIndex {
            block: logical_block,
            leaf_lo: index_block_start + i as u32,
            leaf_hi: 0,
            unused: 0,
        });
    }

    // Write the inode's block field: header + index entries.
    let mut buf = [0u8; INODE_BLOCK_SIZE];
    let header = ExtentHeader {
        magic: EXTENT_HEADER_MAGIC,
        entries: num_index_blocks as u16,
        max: 4,
        depth: 1,
        generation: 0,
    };
    header.write_to(&mut buf[..ExtentHeader::SIZE]);

    for (i, idx) in indices.iter().enumerate() {
        let off = ExtentHeader::SIZE + i * ExtentIndex::SIZE;
        idx.write_to(&mut buf[off..off + ExtentIndex::SIZE]);
    }

    inode.block = buf;

    // Write the leaf blocks to the output.
    for i in 0..num_index_blocks as usize {
        let leaf_start = i * leaves_per_block;
        let leaf_end = ((i + 1) * leaves_per_block).min(all_leaves.len());
        let block_leaves = &all_leaves[leaf_start..leaf_end];

        let mut block_buf = vec![0u8; block_size as usize];

        // Header for this leaf block.
        let leaf_header = ExtentHeader {
            magic: EXTENT_HEADER_MAGIC,
            entries: block_leaves.len() as u16,
            max: leaves_per_block as u16,
            depth: 0,
            generation: 0,
        };
        leaf_header.write_to(&mut block_buf[..ExtentHeader::SIZE]);

        // Leaf entries.
        for (j, leaf) in block_leaves.iter().enumerate() {
            let off = ExtentHeader::SIZE + j * ExtentLeaf::SIZE;
            leaf.write_to(&mut block_buf[off..off + ExtentLeaf::SIZE]);
        }

        // Tail checksum (zeroed -- no metadata checksumming in this implementation).
        let tail = ExtentTail { checksum: 0 };
        let tail_off = block_size as usize - ExtentTail::SIZE;
        tail.write_to(&mut block_buf[tail_off..tail_off + ExtentTail::SIZE]);

        // Seek to the correct block position and write.
        let byte_offset = (*current_block as u64) * (block_size as u64);
        writer.seek(SeekFrom::Start(byte_offset))?;
        writer.write_all(&block_buf)?;

        *current_block += 1;
    }

    // blocks_lo accounts for data blocks plus the index (metadata) blocks.
    let total_blocks = data_blocks + num_index_blocks;
    if inode.flags & inode_flags::HUGE_FILE != 0 {
        inode.blocks_lo = total_blocks;
    } else {
        let sectors = (total_blocks as u64) * (block_size as u64 / 512);
        inode.blocks_lo = sectors as u32;
    }
    inode.flags |= inode_flags::EXTENTS;

    Ok(())
}

// ---------------------------------------------------------------------------
// Read path (Reader)
// ---------------------------------------------------------------------------

/// Parse the extent tree from an inode's `block` field.
///
/// Returns a list of `(physical_start, physical_end)` block ranges covering
/// the file's data.  Supports depth 0 (inline leaves) and depth 1 (one level
/// of index nodes).
pub fn parse_extents<R: Read + Seek>(
    inode: &Inode,
    block_size: u64,
    reader: &mut R,
) -> Result<Vec<(u32, u32)>, crate::error::ReadError> {
    let header = ExtentHeader::read_from(&inode.block);

    if header.magic != EXTENT_HEADER_MAGIC {
        // No valid extent tree (e.g. fast symlinks store the target directly
        // in the block field).  Return an empty list, matching Apple's behavior.
        return Ok(Vec::new());
    }

    match header.depth {
        0 => parse_depth0(&inode.block, &header),
        1 => parse_depth1(&inode.block, &header, block_size, reader),
        _ => Err(crate::error::ReadError::DeepExtentsUnsupported),
    }
}

/// Parse inline leaf entries (depth 0).
fn parse_depth0(
    block_field: &[u8],
    header: &ExtentHeader,
) -> Result<Vec<(u32, u32)>, crate::error::ReadError> {
    let mut ranges = Vec::with_capacity(header.entries as usize);

    for i in 0..header.entries as usize {
        let off = ExtentHeader::SIZE + i * ExtentLeaf::SIZE;
        if off + ExtentLeaf::SIZE > block_field.len() {
            return Err(crate::error::ReadError::InvalidExtents);
        }
        let leaf = ExtentLeaf::read_from(&block_field[off..]);
        let phys_start = leaf.start_lo;
        let phys_end = phys_start + leaf.len as u32;
        ranges.push((phys_start, phys_end));
    }

    Ok(ranges)
}

/// Parse depth-1 extent tree: read index entries from the inode, then read
/// each leaf block from disk.
fn parse_depth1<R: Read + Seek>(
    block_field: &[u8],
    header: &ExtentHeader,
    block_size: u64,
    reader: &mut R,
) -> Result<Vec<(u32, u32)>, crate::error::ReadError> {
    let mut ranges = Vec::new();

    for i in 0..header.entries as usize {
        let off = ExtentHeader::SIZE + i * ExtentIndex::SIZE;
        if off + ExtentIndex::SIZE > block_field.len() {
            return Err(crate::error::ReadError::InvalidExtents);
        }
        let index = ExtentIndex::read_from(&block_field[off..]);

        // Read the leaf block from disk.
        let phys_block = index.leaf() as u64;
        let byte_offset = phys_block * block_size;

        reader.seek(SeekFrom::Start(byte_offset))
            .map_err(|_| crate::error::ReadError::CouldNotReadBlock(phys_block as u32))?;

        let mut leaf_buf = vec![0u8; block_size as usize];
        reader.read_exact(&mut leaf_buf)
            .map_err(|_| crate::error::ReadError::CouldNotReadBlock(phys_block as u32))?;

        let leaf_header = ExtentHeader::read_from(&leaf_buf);
        if leaf_header.magic != EXTENT_HEADER_MAGIC || leaf_header.depth != 0 {
            return Err(crate::error::ReadError::InvalidExtents);
        }

        for j in 0..leaf_header.entries as usize {
            let leaf_off = ExtentHeader::SIZE + j * ExtentLeaf::SIZE;
            if leaf_off + ExtentLeaf::SIZE > leaf_buf.len() {
                return Err(crate::error::ReadError::InvalidExtents);
            }
            let leaf = ExtentLeaf::read_from(&leaf_buf[leaf_off..]);
            let phys_start = leaf.start_lo;
            let phys_end = phys_start + leaf.len as u32;
            ranges.push((phys_start, phys_end));
        }
    }

    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_fill_extent_leaves_single() {
        let leaves = fill_extent_leaves(100, 10, 1, 0);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].block, 0);
        assert_eq!(leaves[0].start_lo, 100);
        assert_eq!(leaves[0].len, 10);
    }

    #[test]
    fn test_fill_extent_leaves_multiple() {
        // 0x8000 * 2 + 5 = 65541 blocks requiring 3 extents.
        let num_blocks = MAX_BLOCKS_PER_EXTENT * 2 + 5;
        let leaves = fill_extent_leaves(0, num_blocks, 3, 0);
        assert_eq!(leaves.len(), 3);
        assert_eq!(leaves[0].len, MAX_BLOCKS_PER_EXTENT as u16);
        assert_eq!(leaves[1].len, MAX_BLOCKS_PER_EXTENT as u16);
        assert_eq!(leaves[2].len, 5);
        assert_eq!(leaves[1].start_lo, MAX_BLOCKS_PER_EXTENT);
        assert_eq!(leaves[2].start_lo, MAX_BLOCKS_PER_EXTENT * 2);
    }

    #[test]
    fn test_inline_extents_roundtrip() {
        let block_size = 4096u32;
        let mut inode = Inode::default();
        let blocks = BlockRange { start: 50, end: 60 };

        let mut cursor = Cursor::new(Vec::new());
        let mut current_block = 100u32;

        write_extents(&mut inode, blocks, block_size, &mut cursor, &mut current_block).unwrap();

        // Should have written nothing to the cursor (all inline).
        assert_eq!(cursor.get_ref().len(), 0);

        // Parse back.
        let ranges = parse_extents(&inode, block_size as u64, &mut cursor).unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], (50, 60));
    }

    #[test]
    fn test_zero_blocks_is_noop() {
        let mut inode = Inode::default();
        let blocks = BlockRange { start: 0, end: 0 };
        let mut cursor = Cursor::new(Vec::new());
        let mut current_block = 0u32;

        write_extents(&mut inode, blocks, 4096, &mut cursor, &mut current_block).unwrap();
        assert_eq!(inode.flags & inode_flags::EXTENTS, 0);
    }

    /// Depth-1 extent tree roundtrip: 5+ extents require an indexed tree
    /// because the inode's 60-byte block field only fits 4 inline leaf entries.
    /// We write the extents, then parse them back and verify the ranges match.
    #[test]
    fn test_depth1_extent_tree_roundtrip() {
        let block_size = 4096u32;
        // 5 extents requires 5 * MAX_BLOCKS_PER_EXTENT data blocks.
        let data_blocks = MAX_BLOCKS_PER_EXTENT * 5;
        let phys_start = 200u32;

        let mut inode = Inode::default();
        let blocks = BlockRange {
            start: phys_start,
            end: phys_start + data_blocks,
        };

        // Allocate a cursor large enough for the index (leaf) blocks.
        // The index blocks are written at current_block * block_size.
        // We need at most a few blocks for the tree, so 1 MiB is plenty.
        let backing = vec![0u8; 1024 * 1024];
        let mut cursor = Cursor::new(backing);
        // Set current_block high enough that the index blocks don't overlap
        // with the range [phys_start .. phys_start + data_blocks].
        let mut current_block = phys_start + data_blocks + 10;

        write_extents(
            &mut inode,
            blocks,
            block_size,
            &mut cursor,
            &mut current_block,
        )
        .unwrap();

        // The inode should have the EXTENTS flag.
        assert_ne!(inode.flags & inode_flags::EXTENTS, 0);

        // The header in the inode should have depth 1.
        let header = ExtentHeader::read_from(&inode.block);
        assert_eq!(header.magic, EXTENT_HEADER_MAGIC);
        assert_eq!(header.depth, 1);
        // There should be at least 1 index entry.
        assert!(header.entries >= 1);

        // Parse back and verify we get 5 ranges, each MAX_BLOCKS_PER_EXTENT long.
        let ranges = parse_extents(&inode, block_size as u64, &mut cursor).unwrap();
        assert_eq!(ranges.len(), 5);

        let mut expected_phys = phys_start;
        for (i, &(start, end)) in ranges.iter().enumerate() {
            assert_eq!(
                start, expected_phys,
                "extent {} start mismatch: expected {} got {}",
                i, expected_phys, start
            );
            assert_eq!(
                end - start,
                MAX_BLOCKS_PER_EXTENT,
                "extent {} length mismatch",
                i
            );
            expected_phys += MAX_BLOCKS_PER_EXTENT;
        }
    }

    /// Depth-1 with a non-even split: 6 extents where the last is shorter.
    #[test]
    fn test_depth1_extent_tree_uneven() {
        let block_size = 4096u32;
        let extra = 42u32;
        let data_blocks = MAX_BLOCKS_PER_EXTENT * 5 + extra;
        let phys_start = 100u32;

        let mut inode = Inode::default();
        let blocks = BlockRange {
            start: phys_start,
            end: phys_start + data_blocks,
        };

        let backing = vec![0u8; 1024 * 1024];
        let mut cursor = Cursor::new(backing);
        let mut current_block = phys_start + data_blocks + 10;

        write_extents(
            &mut inode,
            blocks,
            block_size,
            &mut cursor,
            &mut current_block,
        )
        .unwrap();

        let ranges = parse_extents(&inode, block_size as u64, &mut cursor).unwrap();
        assert_eq!(ranges.len(), 6);

        // First 5 extents should be full-size.
        for i in 0..5 {
            assert_eq!(
                ranges[i].1 - ranges[i].0,
                MAX_BLOCKS_PER_EXTENT,
                "extent {} should be full",
                i
            );
        }
        // Last extent should be the remainder.
        assert_eq!(ranges[5].1 - ranges[5].0, extra);

        // Verify physical contiguity.
        let mut expected_phys = phys_start;
        for &(start, end) in &ranges {
            assert_eq!(start, expected_phys);
            expected_phys = end;
        }
    }
}
