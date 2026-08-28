// Extended attribute handling.
//
// Provides name compression / decompression (prefix stripping), size
// calculations, hashing, and serialization for both inline (inode) and
// block-level xattrs.

use crate::constants::*;
use crate::error::FormatError;

// ---------------------------------------------------------------------------
// Prefix table
// ---------------------------------------------------------------------------

/// Known xattr name prefixes.  The index is stored on disk; the prefix string
/// is stripped from (or prepended to) the attribute name.
const XATTR_PREFIXES: &[(u8, &str)] = &[
    (1, "user."),
    (2, "system.posix_acl_access"),
    (3, "system.posix_acl_default"),
    (4, "trusted."),
    (6, "security."),
    (7, "system."),
    (8, "system.richacl"),
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Round `n` up to the next multiple of `align`.
#[inline]
fn align_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

// ---------------------------------------------------------------------------
// ExtendedAttribute
// ---------------------------------------------------------------------------

/// A single extended attribute, with the name already compressed (prefix
/// stripped) for on-disk storage.
#[derive(Debug, Clone)]
pub struct ExtendedAttribute {
    /// The name with its prefix stripped (the "suffix").
    pub name: String,
    /// Prefix index (see `XATTR_PREFIXES`).
    pub index: u8,
    /// Raw attribute value.
    pub value: Vec<u8>,
}

impl ExtendedAttribute {
    /// Create from a full attribute name (e.g. "user.mime_type") and value.
    /// The name is automatically compressed by finding the longest matching
    /// prefix.
    pub fn new(full_name: &str, value: Vec<u8>) -> Self {
        let (index, suffix) = Self::compress_name(full_name);
        Self {
            name: suffix,
            index,
            value,
        }
    }

    /// Compress an attribute name by finding the longest matching prefix.
    /// Returns `(prefix_index, suffix)`.  If no prefix matches, index 0 is
    /// returned and the full name is kept.
    pub fn compress_name(name: &str) -> (u8, String) {
        let mut best_index = 0u8;
        let mut best_prefix_len = 0usize;

        for &(idx, prefix) in XATTR_PREFIXES {
            if name.starts_with(prefix) && prefix.len() > best_prefix_len {
                best_index = idx;
                best_prefix_len = prefix.len();
            }
        }

        let suffix = &name[best_prefix_len..];
        (best_index, suffix.to_string())
    }

    /// Reconstruct the full attribute name from a prefix index and suffix.
    pub fn decompress_name(index: u8, suffix: &str) -> String {
        for &(idx, prefix) in XATTR_PREFIXES {
            if idx == index {
                return format!("{}{}", prefix, suffix);
            }
        }
        // Unknown index -- return the suffix as-is.
        suffix.to_string()
    }

    /// On-disk size of the entry header + name (aligned to 4 bytes).
    /// The entry header (XAttrEntry) is 16 bytes.
    pub fn entry_size(&self) -> u32 {
        align_up(self.name.len() + 16, 4) as u32
    }

    /// On-disk size of the value (aligned to 4 bytes).
    pub fn value_size(&self) -> u32 {
        align_up(self.value.len(), 4) as u32
    }

    /// Total on-disk footprint: entry + value.
    pub fn total_size(&self) -> u32 {
        self.entry_size() + self.value_size()
    }

    /// Compute the ext4 xattr hash for this attribute.
    ///
    /// The hash covers the name (byte-by-byte mixed into a rolling hash) and
    /// the value (word-by-word).  This matches the kernel's
    /// `ext4_xattr_hash_entry` algorithm.
    pub fn hash(&self) -> u32 {
        // Hash the name.
        let mut h = 0u32;
        for &b in self.name.as_bytes() {
            h = (h << NAME_HASH_SHIFT) ^ (h >> (8 * 4 - NAME_HASH_SHIFT)) ^ (b as u32);
        }

        // Mix in the value, processing it as little-endian u32 words.
        // Partial trailing bytes are handled by zero-padding.
        let value = &self.value;
        let full_words = value.len() / 4;
        for i in 0..full_words {
            let off = i * 4;
            let word = u32::from_le_bytes([
                value[off],
                value[off + 1],
                value[off + 2],
                value[off + 3],
            ]);
            h = (h << VALUE_HASH_SHIFT) ^ (h >> (8 * 4 - VALUE_HASH_SHIFT)) ^ word;
        }

        // Handle trailing bytes (if any).
        let tail = value.len() % 4;
        if tail > 0 {
            let off = full_words * 4;
            let mut bytes = [0u8; 4];
            bytes[..tail].copy_from_slice(&value[off..]);
            let word = u32::from_le_bytes(bytes);
            h = (h << VALUE_HASH_SHIFT) ^ (h >> (8 * 4 - VALUE_HASH_SHIFT)) ^ word;
        }

        h
    }
}

/// Shift amount used when hashing attribute name bytes.
const NAME_HASH_SHIFT: u32 = 5;
/// Shift amount used when hashing attribute value words.
const VALUE_HASH_SHIFT: u32 = 16;

// ---------------------------------------------------------------------------
// XattrState
// ---------------------------------------------------------------------------

/// Tracks which extended attributes go inline (in the inode's extra space)
/// versus in a separate xattr block.
pub struct XattrState {
    /// Capacity for inline xattrs (typically `INODE_EXTRA_SIZE` = 96).
    inode_capacity: u32,
    /// Capacity for block xattrs (one full filesystem block).
    block_capacity: u32,
    /// Attributes assigned to inline storage.
    inline_attrs: Vec<ExtendedAttribute>,
    /// Attributes assigned to block storage.
    block_attrs: Vec<ExtendedAttribute>,
    /// Bytes consumed in the inline area (includes the 4-byte magic header).
    used_inline: u32,
    /// Bytes consumed in the block area (includes the 32-byte block header).
    used_block: u32,
    /// The inode that owns these xattrs (for error reporting).
    inode_number: u32,
}

impl XattrState {
    /// Create a new xattr state tracker for the given inode.
    pub fn new(inode: u32, inode_capacity: u32, block_capacity: u32) -> Self {
        Self {
            inode_capacity,
            block_capacity,
            inline_attrs: Vec::new(),
            block_attrs: Vec::new(),
            // The inline area starts with a 4-byte magic header.
            used_inline: XATTR_INODE_HEADER_SIZE,
            // The block area starts with a 32-byte header.
            used_block: XATTR_BLOCK_HEADER_SIZE,
            inode_number: inode,
        }
    }

    /// Add an attribute.  It is placed inline if there is room; otherwise it
    /// goes into the block area.  Returns an error if neither has enough space.
    pub fn add(&mut self, attr: ExtendedAttribute) -> Result<(), FormatError> {
        let total = attr.total_size();

        // Try inline first.  Reserve 4 bytes for the null terminator that
        // marks the end of the entry list (required by ext4 readers).
        if self.used_inline + total + 4 <= self.inode_capacity {
            self.used_inline += total;
            self.inline_attrs.push(attr);
            return Ok(());
        }

        // Fall back to block.  Same 4-byte null terminator reservation.
        if self.used_block + total + 4 <= self.block_capacity {
            self.used_block += total;
            self.block_attrs.push(attr);
            return Ok(());
        }

        Err(FormatError::XattrInsufficientSpace(self.inode_number))
    }

    /// Whether any inline xattrs have been recorded.
    pub fn has_inline(&self) -> bool {
        !self.inline_attrs.is_empty()
    }

    /// Whether any block xattrs have been recorded.
    pub fn has_block(&self) -> bool {
        !self.block_attrs.is_empty()
    }

    /// Serialize the inline xattrs into a buffer suitable for the inode's
    /// inline xattr area.
    ///
    /// The returned buffer is exactly `inode_capacity` bytes.  Layout:
    ///   - 4-byte magic (`XATTR_HEADER_MAGIC`)
    ///   - Entries packed from the front (header + name + padding)
    ///   - Values packed from the back (aligned to 4 bytes)
    pub fn write_inline(&self) -> Result<Vec<u8>, FormatError> {
        let capacity = self.inode_capacity as usize;
        let mut buf = vec![0u8; capacity];

        // Write the 4-byte magic.
        buf[0..4].copy_from_slice(&XATTR_HEADER_MAGIC.to_le_bytes());

        let mut entry_offset = XATTR_INODE_HEADER_SIZE as usize;
        let mut value_end = capacity;

        for attr in &self.inline_attrs {
            // Place the value at the back, aligned down to 4 bytes.
            let val_size_aligned = align_up(attr.value.len(), 4);
            value_end -= val_size_aligned;

            // The value_offset field is relative to the start of the first
            // entry (i.e. right after the 4-byte magic header for inline attrs).
            let rel_value_offset = value_end - XATTR_INODE_HEADER_SIZE as usize;

            // Write the entry header.
            if entry_offset + 16 + attr.name.len() > buf.len() {
                return Err(FormatError::MalformedXattrBuffer);
            }
            write_xattr_entry(
                &mut buf[entry_offset..],
                &attr.name,
                attr.index,
                rel_value_offset as u16,
                attr.value.len() as u32,
                0, // Hash is 0 for inline xattrs.
            );
            entry_offset += align_up(16 + attr.name.len(), 4);

            // Write the value.
            buf[value_end..value_end + attr.value.len()].copy_from_slice(&attr.value);
        }

        Ok(buf)
    }

    /// Serialize the block xattrs into a full block-sized buffer.
    ///
    /// Layout:
    ///   - 32-byte header (magic + refcount=1 + blocks=1 + 20 zero bytes)
    ///   - Entries sorted by (index, name_len, name), packed from the front
    ///   - Values packed from the back
    pub fn write_block(&self) -> Result<Vec<u8>, FormatError> {
        let capacity = self.block_capacity as usize;
        let mut buf = vec![0u8; capacity];

        // Write the 32-byte block header.
        buf[0..4].copy_from_slice(&XATTR_HEADER_MAGIC.to_le_bytes());
        // refcount = 1
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        // blocks = 1
        buf[8..12].copy_from_slice(&1u32.to_le_bytes());
        // Bytes 12..32 are zero (already zeroed).

        // Sort attributes by (index, name_len, name) for deterministic output.
        let mut sorted: Vec<&ExtendedAttribute> = self.block_attrs.iter().collect();
        sorted.sort_by(|a, b| {
            a.index.cmp(&b.index)
                .then_with(|| a.name.len().cmp(&b.name.len()))
                .then_with(|| a.name.cmp(&b.name))
        });

        let mut entry_offset = XATTR_BLOCK_HEADER_SIZE as usize;
        let mut value_end = capacity;

        for attr in &sorted {
            // Place the value at the back.
            let val_size_aligned = align_up(attr.value.len(), 4);
            value_end -= val_size_aligned;

            // For block xattrs, value_offset is relative to the start of the
            // block (absolute offset within the block buffer).
            let rel_value_offset = value_end;

            if entry_offset + 16 + attr.name.len() > buf.len() {
                return Err(FormatError::MalformedXattrBuffer);
            }
            write_xattr_entry(
                &mut buf[entry_offset..],
                &attr.name,
                attr.index,
                rel_value_offset as u16,
                attr.value.len() as u32,
                attr.hash(),
            );
            entry_offset += align_up(16 + attr.name.len(), 4);

            // Write the value.
            buf[value_end..value_end + attr.value.len()].copy_from_slice(&attr.value);
        }

        Ok(buf)
    }
}

/// Write a single xattr entry (16-byte header + name + padding) into `buf`.
fn write_xattr_entry(
    buf: &mut [u8],
    name: &str,
    name_index: u8,
    value_offset: u16,
    value_size: u32,
    hash: u32,
) {
    let name_bytes = name.as_bytes();

    // name_len (1 byte)
    buf[0] = name_bytes.len() as u8;
    // name_index (1 byte)
    buf[1] = name_index;
    // value_offset (2 bytes LE)
    buf[2..4].copy_from_slice(&value_offset.to_le_bytes());
    // value_inum (4 bytes LE) -- always 0
    buf[4..8].copy_from_slice(&0u32.to_le_bytes());
    // value_size (4 bytes LE)
    buf[8..12].copy_from_slice(&value_size.to_le_bytes());
    // hash (4 bytes LE)
    buf[12..16].copy_from_slice(&hash.to_le_bytes());
    // name
    buf[16..16 + name_bytes.len()].copy_from_slice(name_bytes);
    // Padding is already zero (buffer was zeroed on creation).
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_name_user_prefix() {
        let (idx, suffix) = ExtendedAttribute::compress_name("user.mime_type");
        assert_eq!(idx, 1);
        assert_eq!(suffix, "mime_type");
    }

    #[test]
    fn test_compress_name_security_prefix() {
        let (idx, suffix) = ExtendedAttribute::compress_name("security.selinux");
        assert_eq!(idx, 6);
        assert_eq!(suffix, "selinux");
    }

    #[test]
    fn test_compress_name_system_posix_acl() {
        // "system.posix_acl_access" is an exact match for index 2, which is
        // longer than the generic "system." prefix (index 7).
        let (idx, suffix) = ExtendedAttribute::compress_name("system.posix_acl_access");
        assert_eq!(idx, 2);
        assert_eq!(suffix, "");
    }

    #[test]
    fn test_compress_name_system_generic() {
        let (idx, suffix) = ExtendedAttribute::compress_name("system.something");
        assert_eq!(idx, 7);
        assert_eq!(suffix, "something");
    }

    #[test]
    fn test_compress_name_no_match() {
        let (idx, suffix) = ExtendedAttribute::compress_name("unknown.attr");
        assert_eq!(idx, 0);
        assert_eq!(suffix, "unknown.attr");
    }

    #[test]
    fn test_decompress_name() {
        assert_eq!(
            ExtendedAttribute::decompress_name(1, "mime_type"),
            "user.mime_type"
        );
        assert_eq!(
            ExtendedAttribute::decompress_name(6, "selinux"),
            "security.selinux"
        );
        assert_eq!(
            ExtendedAttribute::decompress_name(2, ""),
            "system.posix_acl_access"
        );
        // Unknown index returns suffix as-is.
        assert_eq!(
            ExtendedAttribute::decompress_name(99, "foo"),
            "foo"
        );
    }

    #[test]
    fn test_entry_and_value_sizes() {
        let attr = ExtendedAttribute::new("user.x", vec![0u8; 10]);
        // name = "x" (1 byte), entry header = 16 bytes -> align_up(17, 4) = 20
        assert_eq!(attr.entry_size(), 20);
        // value = 10 bytes -> align_up(10, 4) = 12
        assert_eq!(attr.value_size(), 12);
        assert_eq!(attr.total_size(), 32);
    }

    #[test]
    fn test_hash_deterministic() {
        let attr = ExtendedAttribute::new("user.test", b"hello".to_vec());
        let h1 = attr.hash();
        let h2 = attr.hash();
        assert_eq!(h1, h2);
        assert_ne!(h1, 0);
    }

    #[test]
    fn test_xattr_state_inline() {
        let mut state = XattrState::new(11, INODE_EXTRA_SIZE, 4096);
        let attr = ExtendedAttribute::new("user.x", vec![1, 2, 3]);
        state.add(attr).unwrap();

        assert!(state.has_inline());
        assert!(!state.has_block());

        let buf = state.write_inline().unwrap();
        assert_eq!(buf.len(), INODE_EXTRA_SIZE as usize);
        // First 4 bytes should be the magic.
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(magic, XATTR_HEADER_MAGIC);
    }

    #[test]
    fn test_xattr_state_overflow_to_block() {
        // Use a tiny inline capacity so everything overflows to block.
        let mut state = XattrState::new(11, XATTR_INODE_HEADER_SIZE, 4096);
        let attr = ExtendedAttribute::new("user.large", vec![0u8; 100]);
        state.add(attr).unwrap();

        assert!(!state.has_inline());
        assert!(state.has_block());

        let buf = state.write_block().unwrap();
        assert_eq!(buf.len(), 4096);
        // First 4 bytes: magic.
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(magic, XATTR_HEADER_MAGIC);
        // Bytes 4..8: refcount = 1.
        let refcount = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(refcount, 1);
    }

    #[test]
    fn test_xattr_state_insufficient_space() {
        // Both inline and block are too small.
        let mut state = XattrState::new(11, XATTR_INODE_HEADER_SIZE, XATTR_BLOCK_HEADER_SIZE);
        let attr = ExtendedAttribute::new("user.big", vec![0u8; 100]);
        let result = state.add(attr);
        assert!(result.is_err());
    }

    #[test]
    fn test_compress_decompress_roundtrip() {
        // Every known prefix should survive a compress -> decompress cycle.
        let names = [
            "user.custom_key",
            "security.selinux",
            "trusted.overlay.opaque",
            "system.posix_acl_access",
            "system.posix_acl_default",
            "system.richacl",
            "system.other",
        ];
        for full_name in names {
            let (idx, suffix) = ExtendedAttribute::compress_name(full_name);
            let reconstructed = ExtendedAttribute::decompress_name(idx, &suffix);
            assert_eq!(
                reconstructed, full_name,
                "roundtrip failed for {full_name}"
            );
        }
    }

    #[test]
    fn test_hash_different_values() {
        // Different values should (almost certainly) produce different hashes.
        let a = ExtendedAttribute::new("user.test", b"value_a".to_vec());
        let b = ExtendedAttribute::new("user.test", b"value_b".to_vec());
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn test_hash_different_names() {
        // Different names with the same value should produce different hashes.
        let a = ExtendedAttribute::new("user.alpha", b"same".to_vec());
        let b = ExtendedAttribute::new("user.beta", b"same".to_vec());
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn test_hash_empty_value() {
        // An empty value should still produce a non-zero hash (from the name).
        let attr = ExtendedAttribute::new("user.empty", Vec::new());
        assert_ne!(attr.hash(), 0);
    }

    #[test]
    fn test_hash_value_with_trailing_bytes() {
        // Value whose length is not a multiple of 4 -- exercises the tail path.
        let attr = ExtendedAttribute::new("user.tail", vec![1, 2, 3, 4, 5]);
        let h = attr.hash();
        assert_ne!(h, 0);
        // Should be deterministic.
        assert_eq!(h, attr.hash());
    }

    #[test]
    fn test_entry_size_alignment() {
        // Name of exact multiple of 4 bytes: "abcd" (4 chars) + 16 header = 20
        // -> align_up(20, 4) = 20.
        let attr = ExtendedAttribute::new("user.abcd", vec![0]);
        assert_eq!(attr.entry_size(), 20);

        // Name of 5 bytes: "abcde" + 16 = 21 -> align_up(21, 4) = 24.
        let attr = ExtendedAttribute::new("user.abcde", vec![0]);
        assert_eq!(attr.entry_size(), 24);

        // Empty suffix: "system.posix_acl_access" compresses to suffix ""
        // -> 0 bytes + 16 = 16 -> aligned = 16.
        let attr = ExtendedAttribute::new("system.posix_acl_access", vec![0]);
        assert_eq!(attr.entry_size(), 16);
    }

    #[test]
    fn test_value_size_alignment() {
        // 0 bytes -> 0.
        let attr = ExtendedAttribute::new("user.x", Vec::new());
        assert_eq!(attr.value_size(), 0);

        // 1 byte -> align_up(1, 4) = 4.
        let attr = ExtendedAttribute::new("user.x", vec![42]);
        assert_eq!(attr.value_size(), 4);

        // 4 bytes -> 4.
        let attr = ExtendedAttribute::new("user.x", vec![0; 4]);
        assert_eq!(attr.value_size(), 4);

        // 5 bytes -> 8.
        let attr = ExtendedAttribute::new("user.x", vec![0; 5]);
        assert_eq!(attr.value_size(), 8);
    }

    #[test]
    fn test_xattr_state_multiple_inline() {
        // Add several small attributes that all fit inline.
        let mut state = XattrState::new(11, INODE_EXTRA_SIZE, 4096);
        state
            .add(ExtendedAttribute::new("user.a", vec![1]))
            .unwrap();
        state
            .add(ExtendedAttribute::new("user.b", vec![2]))
            .unwrap();
        state
            .add(ExtendedAttribute::new("user.c", vec![3]))
            .unwrap();

        assert!(state.has_inline());
        assert!(!state.has_block());

        let buf = state.write_inline().unwrap();
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(magic, XATTR_HEADER_MAGIC);

        // There should be data beyond the header.
        assert!(buf[4..].iter().any(|&b| b != 0));
    }

    #[test]
    fn test_xattr_state_mixed_inline_and_block() {
        // Small inline capacity that fits one tiny attr, then overflow the rest.
        // entry_size("user.a", [1]) = align_up(16 + 1, 4) = 20
        // value_size([1]) = 4
        // total = 24. Inline header = 4. Plus 4-byte null terminator reservation.
        // Need at least 4 + 24 + 4 = 32 bytes inline capacity.
        let inline_cap = 32;
        let mut state = XattrState::new(11, inline_cap, 4096);
        state
            .add(ExtendedAttribute::new("user.a", vec![1]))
            .unwrap();
        // Second attr won't fit inline.
        state
            .add(ExtendedAttribute::new("user.b", vec![2]))
            .unwrap();

        assert!(state.has_inline());
        assert!(state.has_block());
    }

    #[test]
    fn test_write_block_sorted_output() {
        // Attributes should be sorted by (index, name_len, name) in block output.
        let mut state = XattrState::new(11, XATTR_INODE_HEADER_SIZE, 4096);
        // Add in reverse order.
        state
            .add(ExtendedAttribute::new("user.zzz", vec![3]))
            .unwrap();
        state
            .add(ExtendedAttribute::new("security.aaa", vec![1]))
            .unwrap();
        state
            .add(ExtendedAttribute::new("user.aaa", vec![2]))
            .unwrap();

        let buf = state.write_block().unwrap();
        // The block header is 32 bytes. The first entry starts at offset 32.
        // Entry layout: [name_len(1), name_index(1), ...].
        // security (index=6) should come before user (index=1)?
        // Actually ext4 sorts by index numerically: 1 < 6.
        // So user (index=1) entries first, then security (index=6).
        let first_entry_index = buf[32 + 1]; // name_index of first entry
        let second_entry_index = buf[32 + 1 + align_up(16 + 3, 4) as usize]; // "aaa" is 3 bytes
        assert!(
            first_entry_index <= second_entry_index,
            "entries should be sorted by name_index: {} vs {}",
            first_entry_index,
            second_entry_index,
        );
    }

    /// Helper matching the crate-private `align_up` for test assertions.
    fn align_up(n: usize, align: usize) -> usize {
        (n + align - 1) & !(align - 1)
    }
}
