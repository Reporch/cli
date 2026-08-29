// On-disk ext4 structures with manual little-endian serialization.
//
// Every struct matches the canonical ext4 disk layout from
// <https://ext4.wiki.kernel.org/index.php/Ext4_Disk_Layout>.
// No `unsafe`, no zerocopy -- just plain `read_from` / `write_to`.

use crate::constants::*;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Helpers for reading / writing little-endian primitives
// ---------------------------------------------------------------------------

#[inline]
fn get_u8(buf: &[u8], off: usize) -> u8 {
    buf[off]
}

#[inline]
fn put_u8(buf: &mut [u8], off: usize, v: u8) {
    buf[off] = v;
}

#[inline]
fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

#[inline]
fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    let b = v.to_le_bytes();
    buf[off] = b[0];
    buf[off + 1] = b[1];
}

#[inline]
fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    let b = v.to_le_bytes();
    buf[off..off + 4].copy_from_slice(&b);
}

#[inline]
fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

#[inline]
fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    let b = v.to_le_bytes();
    buf[off..off + 8].copy_from_slice(&b);
}

#[inline]
fn get_bytes<const N: usize>(buf: &[u8], off: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&buf[off..off + N]);
    out
}

#[inline]
fn put_bytes(buf: &mut [u8], off: usize, src: &[u8]) {
    buf[off..off + src.len()].copy_from_slice(src);
}

// ---------------------------------------------------------------------------
// Timestamp helper
// ---------------------------------------------------------------------------

/// Return `(seconds_lo, extra)` for the configured reproducible time, falling
/// back to the current wall-clock time when `SOURCE_DATE_EPOCH` is absent.
///
/// * `seconds_lo` -- lower 32 bits of seconds since the Unix epoch.
/// * `extra`      -- upper 2 bits of seconds (epoch bits) in bits 0..1, plus
///                   nanoseconds in bits 2..31.
pub fn timestamp_now() -> (u32, u32) {
    if let Some(value) = std::env::var_os("SOURCE_DATE_EPOCH") {
        let value = value
            .to_str()
            .expect("SOURCE_DATE_EPOCH must be UTF-8")
            .parse::<u64>()
            .expect("SOURCE_DATE_EPOCH must be an integer");
        assert!(value > 0 && value < (1_u64 << 34), "SOURCE_DATE_EPOCH is outside ext4 time range");
        return (value as u32, ((value >> 32) & 0x3) as u32);
    }
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();

    let secs = dur.as_secs();
    let nanos = dur.subsec_nanos();

    let lo = secs as u32; // lower 32 bits (wrapping)
    let epoch_bits = ((secs >> 32) & 0x3) as u32; // 2-bit epoch extension
    let extra = epoch_bits | (nanos << 2);

    (lo, extra)
}

// ===========================================================================
// 1. SuperBlock (1024 bytes)
// ===========================================================================

pub const SUPERBLOCK_SIZE: usize = 1024;

#[derive(Debug, Clone)]
pub struct SuperBlock {
    // -- 0x000 --
    pub inodes_count: u32,
    pub blocks_count_lo: u32,
    pub r_blocks_count_lo: u32,
    pub free_blocks_count_lo: u32,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub log_cluster_size: u32,
    pub blocks_per_group: u32,
    pub clusters_per_group: u32,
    pub inodes_per_group: u32,
    pub mtime: u32,
    pub wtime: u32,
    pub mount_count: u16,
    pub max_mount_count: u16,
    pub magic: u16,
    pub state: u16,
    pub errors: u16,
    pub minor_rev_level: u16,
    pub lastcheck: u32,
    pub check_interval: u32,
    pub creator_os: u32,
    pub rev_level: u32,
    pub def_resuid: u16,
    pub def_resgid: u16,

    // -- 0x054 -- (EXT4_DYNAMIC_REV superblocks only)
    pub first_ino: u32,
    pub inode_size: u16,
    pub block_group_nr: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub volume_name: [u8; 16],
    pub last_mounted: [u8; 64],
    pub algorithm_usage_bitmap: u32,

    // -- 0x0CC --
    pub prealloc_blocks: u8,
    pub prealloc_dir_blocks: u8,
    pub reserved_gdt_blocks: u16,

    // -- 0x0D0 -- journal fields
    pub journal_uuid: [u8; 16],
    pub journal_inum: u32,
    pub journal_dev: u32,
    pub last_orphan: u32,
    pub hash_seed: [u32; 4],
    pub def_hash_version: u8,
    pub journal_backup_type: u8,
    pub desc_size: u16,
    pub default_mount_opts: u32,
    pub first_meta_bg: u32,
    pub mkfs_time: u32,
    pub journal_blocks: [u32; 17],

    // -- 0x150 -- 64-bit support
    pub blocks_count_hi: u32,
    pub r_blocks_count_hi: u32,
    pub free_blocks_count_hi: u32,
    pub min_extra_isize: u16,
    pub want_extra_isize: u16,
    pub flags: u32,
    pub raid_stride: u16,
    pub mmp_interval: u16,
    pub mmp_block: u64,
    pub raid_stripe_width: u32,
    pub log_groups_per_flex: u8,
    pub checksum_type: u8,
    pub reserved_pad: u16,
    pub kbytes_written: u64,

    // -- 0x180 -- snapshot
    pub snapshot_inum: u32,
    pub snapshot_id: u32,
    pub snapshot_r_blocks_count: u64,
    pub snapshot_list: u32,

    // -- 0x194 -- error tracking
    pub error_count: u32,
    pub first_error_time: u32,
    pub first_error_ino: u32,
    pub first_error_block: u64,
    pub first_error_func: [u8; 32],
    pub first_error_line: u32,
    pub last_error_time: u32,
    pub last_error_ino: u32,
    pub last_error_line: u32,
    pub last_error_block: u64,
    pub last_error_func: [u8; 32],

    // -- 0x200 --
    pub mount_opts: [u8; 64],

    // -- 0x240 --
    pub usr_quota_inum: u32,
    pub grp_quota_inum: u32,
    pub overhead_blocks: u32,
    pub backup_bgs: [u32; 2],
    pub encrypt_algos: [u8; 4],
    pub encrypt_pw_salt: [u8; 16],
    pub lpf_ino: u32,
    pub prj_quota_inum: u32,
    pub checksum_seed: u32,

    // -- 0x274 -- high-resolution timestamps
    pub wtime_hi: u8,
    pub mtime_hi: u8,
    pub mkfs_time_hi: u8,
    pub lastcheck_hi: u8,
    pub first_error_time_hi: u8,
    pub last_error_time_hi: u8,
    pub pad: [u8; 2],

    // -- 0x27C --
    pub reserved: [u32; 96],

    // -- 0x3FC --
    pub checksum: u32,
}

impl Default for SuperBlock {
    fn default() -> Self {
        Self {
            inodes_count: 0,
            blocks_count_lo: 0,
            r_blocks_count_lo: 0,
            free_blocks_count_lo: 0,
            free_inodes_count: 0,
            first_data_block: 0,
            log_block_size: 0,
            log_cluster_size: 0,
            blocks_per_group: 0,
            clusters_per_group: 0,
            inodes_per_group: 0,
            mtime: 0,
            wtime: 0,
            mount_count: 0,
            max_mount_count: 0,
            magic: 0,
            state: 0,
            errors: 0,
            minor_rev_level: 0,
            lastcheck: 0,
            check_interval: 0,
            creator_os: 0,
            rev_level: 0,
            def_resuid: 0,
            def_resgid: 0,
            first_ino: 0,
            inode_size: 0,
            block_group_nr: 0,
            feature_compat: 0,
            feature_incompat: 0,
            feature_ro_compat: 0,
            uuid: [0; 16],
            volume_name: [0; 16],
            last_mounted: [0; 64],
            algorithm_usage_bitmap: 0,
            prealloc_blocks: 0,
            prealloc_dir_blocks: 0,
            reserved_gdt_blocks: 0,
            journal_uuid: [0; 16],
            journal_inum: 0,
            journal_dev: 0,
            last_orphan: 0,
            hash_seed: [0; 4],
            def_hash_version: 0,
            journal_backup_type: 0,
            desc_size: 0,
            default_mount_opts: 0,
            first_meta_bg: 0,
            mkfs_time: 0,
            journal_blocks: [0; 17],
            blocks_count_hi: 0,
            r_blocks_count_hi: 0,
            free_blocks_count_hi: 0,
            min_extra_isize: 0,
            want_extra_isize: 0,
            flags: 0,
            raid_stride: 0,
            mmp_interval: 0,
            mmp_block: 0,
            raid_stripe_width: 0,
            log_groups_per_flex: 0,
            checksum_type: 0,
            reserved_pad: 0,
            kbytes_written: 0,
            snapshot_inum: 0,
            snapshot_id: 0,
            snapshot_r_blocks_count: 0,
            snapshot_list: 0,
            error_count: 0,
            first_error_time: 0,
            first_error_ino: 0,
            first_error_block: 0,
            first_error_func: [0; 32],
            first_error_line: 0,
            last_error_time: 0,
            last_error_ino: 0,
            last_error_line: 0,
            last_error_block: 0,
            last_error_func: [0; 32],
            mount_opts: [0; 64],
            usr_quota_inum: 0,
            grp_quota_inum: 0,
            overhead_blocks: 0,
            backup_bgs: [0; 2],
            encrypt_algos: [0; 4],
            encrypt_pw_salt: [0; 16],
            lpf_ino: 0,
            prj_quota_inum: 0,
            checksum_seed: 0,
            wtime_hi: 0,
            mtime_hi: 0,
            mkfs_time_hi: 0,
            lastcheck_hi: 0,
            first_error_time_hi: 0,
            last_error_time_hi: 0,
            pad: [0; 2],
            reserved: [0; 96],
            checksum: 0,
        }
    }
}

impl SuperBlock {
    pub const SIZE: usize = SUPERBLOCK_SIZE;

    /// Deserialize a superblock from a 1024-byte buffer.
    pub fn read_from(buf: &[u8]) -> Self {
        debug_assert!(buf.len() >= Self::SIZE);

        let mut hash_seed = [0u32; 4];
        for i in 0..4 {
            hash_seed[i] = get_u32(buf, 0xEC + i * 4);
        }

        let mut journal_blocks = [0u32; 17];
        for i in 0..17 {
            journal_blocks[i] = get_u32(buf, 0x10C + i * 4);
        }

        let mut backup_bgs = [0u32; 2];
        backup_bgs[0] = get_u32(buf, 0x24C);
        backup_bgs[1] = get_u32(buf, 0x250);

        let mut reserved = [0u32; 96];
        for i in 0..96 {
            reserved[i] = get_u32(buf, 0x27C + i * 4);
        }

        Self {
            inodes_count: get_u32(buf, 0x00),
            blocks_count_lo: get_u32(buf, 0x04),
            r_blocks_count_lo: get_u32(buf, 0x08),
            free_blocks_count_lo: get_u32(buf, 0x0C),
            free_inodes_count: get_u32(buf, 0x10),
            first_data_block: get_u32(buf, 0x14),
            log_block_size: get_u32(buf, 0x18),
            log_cluster_size: get_u32(buf, 0x1C),
            blocks_per_group: get_u32(buf, 0x20),
            clusters_per_group: get_u32(buf, 0x24),
            inodes_per_group: get_u32(buf, 0x28),
            mtime: get_u32(buf, 0x2C),
            wtime: get_u32(buf, 0x30),
            mount_count: get_u16(buf, 0x34),
            max_mount_count: get_u16(buf, 0x36),
            magic: get_u16(buf, 0x38),
            state: get_u16(buf, 0x3A),
            errors: get_u16(buf, 0x3C),
            minor_rev_level: get_u16(buf, 0x3E),
            lastcheck: get_u32(buf, 0x40),
            check_interval: get_u32(buf, 0x44),
            creator_os: get_u32(buf, 0x48),
            rev_level: get_u32(buf, 0x4C),
            def_resuid: get_u16(buf, 0x50),
            def_resgid: get_u16(buf, 0x52),
            first_ino: get_u32(buf, 0x54),
            inode_size: get_u16(buf, 0x58),
            block_group_nr: get_u16(buf, 0x5A),
            feature_compat: get_u32(buf, 0x5C),
            feature_incompat: get_u32(buf, 0x60),
            feature_ro_compat: get_u32(buf, 0x64),
            uuid: get_bytes(buf, 0x68),
            volume_name: get_bytes(buf, 0x78),
            last_mounted: get_bytes(buf, 0x88),
            algorithm_usage_bitmap: get_u32(buf, 0xC8),
            prealloc_blocks: get_u8(buf, 0xCC),
            prealloc_dir_blocks: get_u8(buf, 0xCD),
            reserved_gdt_blocks: get_u16(buf, 0xCE),
            journal_uuid: get_bytes(buf, 0xD0),
            journal_inum: get_u32(buf, 0xE0),
            journal_dev: get_u32(buf, 0xE4),
            last_orphan: get_u32(buf, 0xE8),
            hash_seed,
            def_hash_version: get_u8(buf, 0xFC),
            journal_backup_type: get_u8(buf, 0xFD),
            desc_size: get_u16(buf, 0xFE),
            default_mount_opts: get_u32(buf, 0x100),
            first_meta_bg: get_u32(buf, 0x104),
            mkfs_time: get_u32(buf, 0x108),
            journal_blocks,
            blocks_count_hi: get_u32(buf, 0x150),
            r_blocks_count_hi: get_u32(buf, 0x154),
            free_blocks_count_hi: get_u32(buf, 0x158),
            min_extra_isize: get_u16(buf, 0x15C),
            want_extra_isize: get_u16(buf, 0x15E),
            flags: get_u32(buf, 0x160),
            raid_stride: get_u16(buf, 0x164),
            mmp_interval: get_u16(buf, 0x166),
            mmp_block: get_u64(buf, 0x168),
            raid_stripe_width: get_u32(buf, 0x170),
            log_groups_per_flex: get_u8(buf, 0x174),
            checksum_type: get_u8(buf, 0x175),
            reserved_pad: get_u16(buf, 0x176),
            kbytes_written: get_u64(buf, 0x178),
            snapshot_inum: get_u32(buf, 0x180),
            snapshot_id: get_u32(buf, 0x184),
            snapshot_r_blocks_count: get_u64(buf, 0x188),
            snapshot_list: get_u32(buf, 0x190),
            error_count: get_u32(buf, 0x194),
            first_error_time: get_u32(buf, 0x198),
            first_error_ino: get_u32(buf, 0x19C),
            first_error_block: get_u64(buf, 0x1A0),
            first_error_func: get_bytes(buf, 0x1A8),
            first_error_line: get_u32(buf, 0x1C8),
            last_error_time: get_u32(buf, 0x1CC),
            last_error_ino: get_u32(buf, 0x1D0),
            last_error_line: get_u32(buf, 0x1D4),
            last_error_block: get_u64(buf, 0x1D8),
            last_error_func: get_bytes(buf, 0x1E0),
            mount_opts: get_bytes(buf, 0x200),
            usr_quota_inum: get_u32(buf, 0x240),
            grp_quota_inum: get_u32(buf, 0x244),
            overhead_blocks: get_u32(buf, 0x248),
            backup_bgs,
            encrypt_algos: get_bytes(buf, 0x254),
            encrypt_pw_salt: get_bytes(buf, 0x258),
            lpf_ino: get_u32(buf, 0x268),
            prj_quota_inum: get_u32(buf, 0x26C),
            checksum_seed: get_u32(buf, 0x270),
            wtime_hi: get_u8(buf, 0x274),
            mtime_hi: get_u8(buf, 0x275),
            mkfs_time_hi: get_u8(buf, 0x276),
            lastcheck_hi: get_u8(buf, 0x277),
            first_error_time_hi: get_u8(buf, 0x278),
            last_error_time_hi: get_u8(buf, 0x279),
            pad: get_bytes(buf, 0x27A),
            reserved,
            checksum: get_u32(buf, 0x3FC),
        }
    }

    /// Serialize this superblock into a 1024-byte buffer.
    pub fn write_to(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= Self::SIZE);

        // Zero the entire buffer first so padding / reserved areas are clean.
        buf[..Self::SIZE].fill(0);

        put_u32(buf, 0x00, self.inodes_count);
        put_u32(buf, 0x04, self.blocks_count_lo);
        put_u32(buf, 0x08, self.r_blocks_count_lo);
        put_u32(buf, 0x0C, self.free_blocks_count_lo);
        put_u32(buf, 0x10, self.free_inodes_count);
        put_u32(buf, 0x14, self.first_data_block);
        put_u32(buf, 0x18, self.log_block_size);
        put_u32(buf, 0x1C, self.log_cluster_size);
        put_u32(buf, 0x20, self.blocks_per_group);
        put_u32(buf, 0x24, self.clusters_per_group);
        put_u32(buf, 0x28, self.inodes_per_group);
        put_u32(buf, 0x2C, self.mtime);
        put_u32(buf, 0x30, self.wtime);
        put_u16(buf, 0x34, self.mount_count);
        put_u16(buf, 0x36, self.max_mount_count);
        put_u16(buf, 0x38, self.magic);
        put_u16(buf, 0x3A, self.state);
        put_u16(buf, 0x3C, self.errors);
        put_u16(buf, 0x3E, self.minor_rev_level);
        put_u32(buf, 0x40, self.lastcheck);
        put_u32(buf, 0x44, self.check_interval);
        put_u32(buf, 0x48, self.creator_os);
        put_u32(buf, 0x4C, self.rev_level);
        put_u16(buf, 0x50, self.def_resuid);
        put_u16(buf, 0x52, self.def_resgid);
        put_u32(buf, 0x54, self.first_ino);
        put_u16(buf, 0x58, self.inode_size);
        put_u16(buf, 0x5A, self.block_group_nr);
        put_u32(buf, 0x5C, self.feature_compat);
        put_u32(buf, 0x60, self.feature_incompat);
        put_u32(buf, 0x64, self.feature_ro_compat);
        put_bytes(buf, 0x68, &self.uuid);
        put_bytes(buf, 0x78, &self.volume_name);
        put_bytes(buf, 0x88, &self.last_mounted);
        put_u32(buf, 0xC8, self.algorithm_usage_bitmap);
        put_u8(buf, 0xCC, self.prealloc_blocks);
        put_u8(buf, 0xCD, self.prealloc_dir_blocks);
        put_u16(buf, 0xCE, self.reserved_gdt_blocks);
        put_bytes(buf, 0xD0, &self.journal_uuid);
        put_u32(buf, 0xE0, self.journal_inum);
        put_u32(buf, 0xE4, self.journal_dev);
        put_u32(buf, 0xE8, self.last_orphan);
        for i in 0..4 {
            put_u32(buf, 0xEC + i * 4, self.hash_seed[i]);
        }
        put_u8(buf, 0xFC, self.def_hash_version);
        put_u8(buf, 0xFD, self.journal_backup_type);
        put_u16(buf, 0xFE, self.desc_size);
        put_u32(buf, 0x100, self.default_mount_opts);
        put_u32(buf, 0x104, self.first_meta_bg);
        put_u32(buf, 0x108, self.mkfs_time);
        for i in 0..17 {
            put_u32(buf, 0x10C + i * 4, self.journal_blocks[i]);
        }
        put_u32(buf, 0x150, self.blocks_count_hi);
        put_u32(buf, 0x154, self.r_blocks_count_hi);
        put_u32(buf, 0x158, self.free_blocks_count_hi);
        put_u16(buf, 0x15C, self.min_extra_isize);
        put_u16(buf, 0x15E, self.want_extra_isize);
        put_u32(buf, 0x160, self.flags);
        put_u16(buf, 0x164, self.raid_stride);
        put_u16(buf, 0x166, self.mmp_interval);
        put_u64(buf, 0x168, self.mmp_block);
        put_u32(buf, 0x170, self.raid_stripe_width);
        put_u8(buf, 0x174, self.log_groups_per_flex);
        put_u8(buf, 0x175, self.checksum_type);
        put_u16(buf, 0x176, self.reserved_pad);
        put_u64(buf, 0x178, self.kbytes_written);
        put_u32(buf, 0x180, self.snapshot_inum);
        put_u32(buf, 0x184, self.snapshot_id);
        put_u64(buf, 0x188, self.snapshot_r_blocks_count);
        put_u32(buf, 0x190, self.snapshot_list);
        put_u32(buf, 0x194, self.error_count);
        put_u32(buf, 0x198, self.first_error_time);
        put_u32(buf, 0x19C, self.first_error_ino);
        put_u64(buf, 0x1A0, self.first_error_block);
        put_bytes(buf, 0x1A8, &self.first_error_func);
        put_u32(buf, 0x1C8, self.first_error_line);
        put_u32(buf, 0x1CC, self.last_error_time);
        put_u32(buf, 0x1D0, self.last_error_ino);
        put_u32(buf, 0x1D4, self.last_error_line);
        put_u64(buf, 0x1D8, self.last_error_block);
        put_bytes(buf, 0x1E0, &self.last_error_func);
        put_bytes(buf, 0x200, &self.mount_opts);
        put_u32(buf, 0x240, self.usr_quota_inum);
        put_u32(buf, 0x244, self.grp_quota_inum);
        put_u32(buf, 0x248, self.overhead_blocks);
        put_u32(buf, 0x24C, self.backup_bgs[0]);
        put_u32(buf, 0x250, self.backup_bgs[1]);
        put_bytes(buf, 0x254, &self.encrypt_algos);
        put_bytes(buf, 0x258, &self.encrypt_pw_salt);
        put_u32(buf, 0x268, self.lpf_ino);
        put_u32(buf, 0x26C, self.prj_quota_inum);
        put_u32(buf, 0x270, self.checksum_seed);
        put_u8(buf, 0x274, self.wtime_hi);
        put_u8(buf, 0x275, self.mtime_hi);
        put_u8(buf, 0x276, self.mkfs_time_hi);
        put_u8(buf, 0x277, self.lastcheck_hi);
        put_u8(buf, 0x278, self.first_error_time_hi);
        put_u8(buf, 0x279, self.last_error_time_hi);
        put_bytes(buf, 0x27A, &self.pad);
        for i in 0..96 {
            put_u32(buf, 0x27C + i * 4, self.reserved[i]);
        }
        put_u32(buf, 0x3FC, self.checksum);
    }
}

// ===========================================================================
// 2. GroupDescriptor (32 bytes -- the basic descriptor without 64-bit fields)
// ===========================================================================

/// Block group descriptor (32-byte variant).
///
/// When the `INCOMPAT_64BIT` feature is set *and* `desc_size >= 64`, the
/// kernel uses an extended 64-byte descriptor.  We keep the 32-byte base
/// here; callers can layer the hi-word fields on top when needed.
#[derive(Debug, Clone, Default)]
pub struct GroupDescriptor {
    pub block_bitmap_lo: u32,
    pub inode_bitmap_lo: u32,
    pub inode_table_lo: u32,
    pub free_blocks_count_lo: u16,
    pub free_inodes_count_lo: u16,
    pub used_dirs_count_lo: u16,
    pub flags: u16,
    pub exclude_bitmap_lo: u32,
    pub block_bitmap_csum_lo: u16,
    pub inode_bitmap_csum_lo: u16,
    pub itable_unused_lo: u16,
    pub checksum: u16,
}

impl GroupDescriptor {
    pub const SIZE: usize = 32;

    pub fn read_from(buf: &[u8]) -> Self {
        debug_assert!(buf.len() >= Self::SIZE);
        Self {
            block_bitmap_lo: get_u32(buf, 0x00),
            inode_bitmap_lo: get_u32(buf, 0x04),
            inode_table_lo: get_u32(buf, 0x08),
            free_blocks_count_lo: get_u16(buf, 0x0C),
            free_inodes_count_lo: get_u16(buf, 0x0E),
            used_dirs_count_lo: get_u16(buf, 0x10),
            flags: get_u16(buf, 0x12),
            exclude_bitmap_lo: get_u32(buf, 0x14),
            block_bitmap_csum_lo: get_u16(buf, 0x18),
            inode_bitmap_csum_lo: get_u16(buf, 0x1A),
            itable_unused_lo: get_u16(buf, 0x1C),
            checksum: get_u16(buf, 0x1E),
        }
    }

    pub fn write_to(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= Self::SIZE);
        buf[..Self::SIZE].fill(0);

        put_u32(buf, 0x00, self.block_bitmap_lo);
        put_u32(buf, 0x04, self.inode_bitmap_lo);
        put_u32(buf, 0x08, self.inode_table_lo);
        put_u16(buf, 0x0C, self.free_blocks_count_lo);
        put_u16(buf, 0x0E, self.free_inodes_count_lo);
        put_u16(buf, 0x10, self.used_dirs_count_lo);
        put_u16(buf, 0x12, self.flags);
        put_u32(buf, 0x14, self.exclude_bitmap_lo);
        put_u16(buf, 0x18, self.block_bitmap_csum_lo);
        put_u16(buf, 0x1A, self.inode_bitmap_csum_lo);
        put_u16(buf, 0x1C, self.itable_unused_lo);
        put_u16(buf, 0x1E, self.checksum);
    }
}

// ===========================================================================
// 3. Inode (256 bytes = 160 base + 96 inline xattrs)
// ===========================================================================

#[derive(Debug, Clone)]
pub struct Inode {
    // -- byte 0 --
    pub mode: u16,
    pub uid: u16,
    pub size_lo: u32,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub dtime: u32,
    pub gid: u16,
    pub links_count: u16,
    pub blocks_lo: u32,
    pub flags: u32,
    pub version: u32,

    /// 60-byte block field: extent tree root or inline symlink data.
    pub block: [u8; INODE_BLOCK_SIZE],

    pub generation: u32,
    pub xattr_block_lo: u32,
    pub size_hi: u32,
    pub obsolete_fragment_addr: u32,

    // -- OS-dependent 2 (osd2) --
    pub blocks_hi: u16,
    pub xattr_block_hi: u16,
    pub uid_hi: u16,
    pub gid_hi: u16,
    pub checksum_lo: u16,
    pub reserved: u16,

    // -- extra fields (requires extra_isize >= 32) --
    pub extra_isize: u16,
    pub checksum_hi: u16,
    pub ctime_extra: u32,
    pub mtime_extra: u32,
    pub atime_extra: u32,
    pub crtime: u32,
    pub crtime_extra: u32,
    pub version_hi: u32,
    pub projid: u32,

    /// Inline extended attribute space (fills out to 256 bytes).
    pub inline_xattrs: [u8; 96],
}

impl Default for Inode {
    fn default() -> Self {
        Self {
            mode: 0,
            uid: 0,
            size_lo: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            links_count: 0,
            blocks_lo: 0,
            flags: 0,
            version: 0,
            block: [0; INODE_BLOCK_SIZE],
            generation: 0,
            xattr_block_lo: 0,
            size_hi: 0,
            obsolete_fragment_addr: 0,
            blocks_hi: 0,
            xattr_block_hi: 0,
            uid_hi: 0,
            gid_hi: 0,
            checksum_lo: 0,
            reserved: 0,
            extra_isize: 0,
            checksum_hi: 0,
            ctime_extra: 0,
            mtime_extra: 0,
            atime_extra: 0,
            crtime: 0,
            crtime_extra: 0,
            version_hi: 0,
            projid: 0,
            inline_xattrs: [0; 96],
        }
    }
}

impl Inode {
    /// Total on-disk size (base 160 + 96 bytes inline xattrs = 256).
    pub const SIZE: usize = INODE_SIZE as usize;

    // -- Offset constants within the 160-byte base --------------------------

    const OFF_MODE: usize = 0x00;
    const OFF_UID: usize = 0x02;
    const OFF_SIZE_LO: usize = 0x04;
    const OFF_ATIME: usize = 0x08;
    const OFF_CTIME: usize = 0x0C;
    const OFF_MTIME: usize = 0x10;
    const OFF_DTIME: usize = 0x14;
    const OFF_GID: usize = 0x18;
    const OFF_LINKS: usize = 0x1A;
    const OFF_BLOCKS_LO: usize = 0x1C;
    const OFF_FLAGS: usize = 0x20;
    const OFF_VERSION: usize = 0x24;
    const OFF_BLOCK: usize = 0x28; // 60 bytes
    const OFF_GENERATION: usize = 0x64;
    const OFF_XATTR_LO: usize = 0x68;
    const OFF_SIZE_HI: usize = 0x6C;
    const OFF_FRAG_ADDR: usize = 0x70;
    // osd2
    const OFF_BLOCKS_HI: usize = 0x74;
    const OFF_XATTR_HI: usize = 0x76;
    const OFF_UID_HI: usize = 0x78;
    const OFF_GID_HI: usize = 0x7A;
    const OFF_CSUM_LO: usize = 0x7C;
    const OFF_RESERVED: usize = 0x7E;
    // extra isize region starts at 128
    const OFF_EXTRA_ISIZE: usize = 0x80;
    const OFF_CSUM_HI: usize = 0x82;
    const OFF_CTIME_EXTRA: usize = 0x84;
    const OFF_MTIME_EXTRA: usize = 0x88;
    const OFF_ATIME_EXTRA: usize = 0x8C;
    const OFF_CRTIME: usize = 0x90;
    const OFF_CRTIME_EXTRA: usize = 0x94;
    const OFF_VERSION_HI: usize = 0x98;
    const OFF_PROJID: usize = 0x9C;
    // inline xattrs start at 160
    const OFF_INLINE_XATTRS: usize = INODE_ACTUAL_SIZE as usize;

    // -- Serialization ------------------------------------------------------

    pub fn read_from(buf: &[u8]) -> Self {
        debug_assert!(buf.len() >= Self::SIZE);
        Self {
            mode: get_u16(buf, Self::OFF_MODE),
            uid: get_u16(buf, Self::OFF_UID),
            size_lo: get_u32(buf, Self::OFF_SIZE_LO),
            atime: get_u32(buf, Self::OFF_ATIME),
            ctime: get_u32(buf, Self::OFF_CTIME),
            mtime: get_u32(buf, Self::OFF_MTIME),
            dtime: get_u32(buf, Self::OFF_DTIME),
            gid: get_u16(buf, Self::OFF_GID),
            links_count: get_u16(buf, Self::OFF_LINKS),
            blocks_lo: get_u32(buf, Self::OFF_BLOCKS_LO),
            flags: get_u32(buf, Self::OFF_FLAGS),
            version: get_u32(buf, Self::OFF_VERSION),
            block: get_bytes(buf, Self::OFF_BLOCK),
            generation: get_u32(buf, Self::OFF_GENERATION),
            xattr_block_lo: get_u32(buf, Self::OFF_XATTR_LO),
            size_hi: get_u32(buf, Self::OFF_SIZE_HI),
            obsolete_fragment_addr: get_u32(buf, Self::OFF_FRAG_ADDR),
            blocks_hi: get_u16(buf, Self::OFF_BLOCKS_HI),
            xattr_block_hi: get_u16(buf, Self::OFF_XATTR_HI),
            uid_hi: get_u16(buf, Self::OFF_UID_HI),
            gid_hi: get_u16(buf, Self::OFF_GID_HI),
            checksum_lo: get_u16(buf, Self::OFF_CSUM_LO),
            reserved: get_u16(buf, Self::OFF_RESERVED),
            extra_isize: get_u16(buf, Self::OFF_EXTRA_ISIZE),
            checksum_hi: get_u16(buf, Self::OFF_CSUM_HI),
            ctime_extra: get_u32(buf, Self::OFF_CTIME_EXTRA),
            mtime_extra: get_u32(buf, Self::OFF_MTIME_EXTRA),
            atime_extra: get_u32(buf, Self::OFF_ATIME_EXTRA),
            crtime: get_u32(buf, Self::OFF_CRTIME),
            crtime_extra: get_u32(buf, Self::OFF_CRTIME_EXTRA),
            version_hi: get_u32(buf, Self::OFF_VERSION_HI),
            projid: get_u32(buf, Self::OFF_PROJID),
            inline_xattrs: get_bytes(buf, Self::OFF_INLINE_XATTRS),
        }
    }

    pub fn write_to(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= Self::SIZE);
        buf[..Self::SIZE].fill(0);

        put_u16(buf, Self::OFF_MODE, self.mode);
        put_u16(buf, Self::OFF_UID, self.uid);
        put_u32(buf, Self::OFF_SIZE_LO, self.size_lo);
        put_u32(buf, Self::OFF_ATIME, self.atime);
        put_u32(buf, Self::OFF_CTIME, self.ctime);
        put_u32(buf, Self::OFF_MTIME, self.mtime);
        put_u32(buf, Self::OFF_DTIME, self.dtime);
        put_u16(buf, Self::OFF_GID, self.gid);
        put_u16(buf, Self::OFF_LINKS, self.links_count);
        put_u32(buf, Self::OFF_BLOCKS_LO, self.blocks_lo);
        put_u32(buf, Self::OFF_FLAGS, self.flags);
        put_u32(buf, Self::OFF_VERSION, self.version);
        put_bytes(buf, Self::OFF_BLOCK, &self.block);
        put_u32(buf, Self::OFF_GENERATION, self.generation);
        put_u32(buf, Self::OFF_XATTR_LO, self.xattr_block_lo);
        put_u32(buf, Self::OFF_SIZE_HI, self.size_hi);
        put_u32(buf, Self::OFF_FRAG_ADDR, self.obsolete_fragment_addr);
        put_u16(buf, Self::OFF_BLOCKS_HI, self.blocks_hi);
        put_u16(buf, Self::OFF_XATTR_HI, self.xattr_block_hi);
        put_u16(buf, Self::OFF_UID_HI, self.uid_hi);
        put_u16(buf, Self::OFF_GID_HI, self.gid_hi);
        put_u16(buf, Self::OFF_CSUM_LO, self.checksum_lo);
        put_u16(buf, Self::OFF_RESERVED, self.reserved);
        put_u16(buf, Self::OFF_EXTRA_ISIZE, self.extra_isize);
        put_u16(buf, Self::OFF_CSUM_HI, self.checksum_hi);
        put_u32(buf, Self::OFF_CTIME_EXTRA, self.ctime_extra);
        put_u32(buf, Self::OFF_MTIME_EXTRA, self.mtime_extra);
        put_u32(buf, Self::OFF_ATIME_EXTRA, self.atime_extra);
        put_u32(buf, Self::OFF_CRTIME, self.crtime);
        put_u32(buf, Self::OFF_CRTIME_EXTRA, self.crtime_extra);
        put_u32(buf, Self::OFF_VERSION_HI, self.version_hi);
        put_u32(buf, Self::OFF_PROJID, self.projid);
        put_bytes(buf, Self::OFF_INLINE_XATTRS, &self.inline_xattrs);
    }

    // -- Constructors -------------------------------------------------------

    /// Create the root directory inode (inode 2).
    ///
    /// Sets `S_IFDIR | 0o755`, two links (`.` and `..`), `HUGE_FILE` flag,
    /// and timestamps to the current wall-clock time.
    pub fn root_inode() -> Self {
        let (time_lo, time_extra) = timestamp_now();
        Self {
            mode: file_mode::S_IFDIR | 0o755,
            links_count: 2,
            flags: inode_flags::HUGE_FILE,
            extra_isize: EXTRA_ISIZE,
            atime: time_lo,
            ctime: time_lo,
            mtime: time_lo,
            crtime: time_lo,
            atime_extra: time_extra,
            ctime_extra: time_extra,
            mtime_extra: time_extra,
            crtime_extra: time_extra,
            ..Self::default()
        }
    }

    // -- Type queries -------------------------------------------------------

    /// True if this inode represents a directory.
    #[inline]
    pub fn is_dir(&self) -> bool {
        is_dir(self.mode)
    }

    /// True if this inode represents a regular file.
    #[inline]
    pub fn is_reg(&self) -> bool {
        is_reg(self.mode)
    }

    /// True if this inode represents a symbolic link.
    #[inline]
    pub fn is_link(&self) -> bool {
        is_link(self.mode)
    }

    // -- Size helpers -------------------------------------------------------

    /// Combine `size_lo` and `size_hi` into a 64-bit file size.
    #[inline]
    pub fn file_size(&self) -> u64 {
        (self.size_lo as u64) | ((self.size_hi as u64) << 32)
    }

    /// Set the 64-bit file size, splitting into `size_lo` / `size_hi`.
    #[inline]
    pub fn set_file_size(&mut self, size: u64) {
        self.size_lo = size as u32;
        self.size_hi = (size >> 32) as u32;
    }

    // -- UID / GID helpers --------------------------------------------------

    /// Full 32-bit UID (lo + hi).
    #[inline]
    pub fn uid_full(&self) -> u32 {
        (self.uid as u32) | ((self.uid_hi as u32) << 16)
    }

    /// Full 32-bit GID (lo + hi).
    #[inline]
    pub fn gid_full(&self) -> u32 {
        (self.gid as u32) | ((self.gid_hi as u32) << 16)
    }

    /// Set the full 32-bit UID, splitting into lo / hi.
    #[inline]
    pub fn set_uid(&mut self, uid: u32) {
        self.uid = uid as u16;
        self.uid_hi = (uid >> 16) as u16;
    }

    /// Set the full 32-bit GID, splitting into lo / hi.
    #[inline]
    pub fn set_gid(&mut self, gid: u32) {
        self.gid = gid as u16;
        self.gid_hi = (gid >> 16) as u16;
    }
}

// ===========================================================================
// 4. ExtentHeader (12 bytes)
// ===========================================================================

/// Header of an ext4 extent tree node (root, internal, or leaf).
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtentHeader {
    /// Magic number (`EXTENT_HEADER_MAGIC` = 0xF30A).
    pub magic: u16,
    /// Number of valid entries following this header.
    pub entries: u16,
    /// Maximum number of entries that could follow this header.
    pub max: u16,
    /// Depth of this node in the extent tree (0 = leaf).
    pub depth: u16,
    /// Generation of the tree (used by Lustre, not standard ext4).
    pub generation: u32,
}

impl ExtentHeader {
    pub const SIZE: usize = 12;

    pub fn read_from(buf: &[u8]) -> Self {
        debug_assert!(buf.len() >= Self::SIZE);
        Self {
            magic: get_u16(buf, 0),
            entries: get_u16(buf, 2),
            max: get_u16(buf, 4),
            depth: get_u16(buf, 6),
            generation: get_u32(buf, 8),
        }
    }

    pub fn write_to(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= Self::SIZE);
        put_u16(buf, 0, self.magic);
        put_u16(buf, 2, self.entries);
        put_u16(buf, 4, self.max);
        put_u16(buf, 6, self.depth);
        put_u32(buf, 8, self.generation);
    }
}

// ===========================================================================
// 5. ExtentLeaf (12 bytes)
// ===========================================================================

/// A leaf entry in the extent tree, mapping logical blocks to physical blocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtentLeaf {
    /// First logical block number that this extent covers.
    pub block: u32,
    /// Number of blocks covered by this extent.
    /// If the high bit is set, the extent is uninitialized (pre-allocated).
    pub len: u16,
    /// Upper 16 bits of the 48-bit physical block number.
    pub start_hi: u16,
    /// Lower 32 bits of the 48-bit physical block number.
    pub start_lo: u32,
}

impl ExtentLeaf {
    pub const SIZE: usize = 12;

    pub fn read_from(buf: &[u8]) -> Self {
        debug_assert!(buf.len() >= Self::SIZE);
        Self {
            block: get_u32(buf, 0),
            len: get_u16(buf, 4),
            start_hi: get_u16(buf, 6),
            start_lo: get_u32(buf, 8),
        }
    }

    pub fn write_to(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= Self::SIZE);
        put_u32(buf, 0, self.block);
        put_u16(buf, 4, self.len);
        put_u16(buf, 6, self.start_hi);
        put_u32(buf, 8, self.start_lo);
    }

    /// Full 48-bit physical start block.
    #[inline]
    pub fn start(&self) -> u64 {
        (self.start_lo as u64) | ((self.start_hi as u64) << 32)
    }
}

// ===========================================================================
// 6. ExtentIndex (12 bytes)
// ===========================================================================

/// An internal (index) entry in the extent tree, pointing to the next level.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtentIndex {
    /// Logical block number -- this index covers blocks >= `block`.
    pub block: u32,
    /// Lower 32 bits of the physical block containing the child node.
    pub leaf_lo: u32,
    /// Upper 16 bits of the physical block containing the child node.
    pub leaf_hi: u16,
    pub unused: u16,
}

impl ExtentIndex {
    pub const SIZE: usize = 12;

    pub fn read_from(buf: &[u8]) -> Self {
        debug_assert!(buf.len() >= Self::SIZE);
        Self {
            block: get_u32(buf, 0),
            leaf_lo: get_u32(buf, 4),
            leaf_hi: get_u16(buf, 8),
            unused: get_u16(buf, 10),
        }
    }

    pub fn write_to(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= Self::SIZE);
        put_u32(buf, 0, self.block);
        put_u32(buf, 4, self.leaf_lo);
        put_u16(buf, 8, self.leaf_hi);
        put_u16(buf, 10, self.unused);
    }

    /// Full 48-bit physical block of the child node.
    #[inline]
    pub fn leaf(&self) -> u64 {
        (self.leaf_lo as u64) | ((self.leaf_hi as u64) << 32)
    }
}

// ===========================================================================
// 7. ExtentTail (4 bytes)
// ===========================================================================

/// Checksum appended after the last extent entry in a tree block.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtentTail {
    pub checksum: u32,
}

impl ExtentTail {
    pub const SIZE: usize = 4;

    pub fn read_from(buf: &[u8]) -> Self {
        debug_assert!(buf.len() >= Self::SIZE);
        Self {
            checksum: get_u32(buf, 0),
        }
    }

    pub fn write_to(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= Self::SIZE);
        put_u32(buf, 0, self.checksum);
    }
}

// ===========================================================================
// 8. DirectoryEntry (8-byte header + variable-length name)
// ===========================================================================

/// On-disk directory entry header (the name bytes follow immediately).
#[derive(Debug, Clone, Default)]
pub struct DirectoryEntry {
    /// Inode number this entry refers to (0 = deleted / unused).
    pub inode: u32,
    /// Total size of this directory entry (header + name + padding).
    pub rec_len: u16,
    /// Length of the file name in bytes.
    pub name_len: u8,
    /// File type code (see `FileType`).  Only valid when `INCOMPAT_FILETYPE`
    /// is enabled; otherwise this byte is the high byte of the old `name_len`
    /// field.
    pub file_type: u8,
}

impl DirectoryEntry {
    /// Size of the fixed header (name bytes are *not* included).
    pub const SIZE: usize = 8;

    pub fn read_from(buf: &[u8]) -> Self {
        debug_assert!(buf.len() >= Self::SIZE);
        Self {
            inode: get_u32(buf, 0),
            rec_len: get_u16(buf, 4),
            name_len: get_u8(buf, 6),
            file_type: get_u8(buf, 7),
        }
    }

    pub fn write_to(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= Self::SIZE);
        put_u32(buf, 0, self.inode);
        put_u16(buf, 4, self.rec_len);
        put_u8(buf, 6, self.name_len);
        put_u8(buf, 7, self.file_type);
    }
}

// ===========================================================================
// 9. XAttrEntry (16-byte header + variable-length name)
// ===========================================================================

/// On-disk extended-attribute entry.  The name bytes follow immediately after
/// this header, then padding to a 4-byte boundary.
#[derive(Debug, Clone, Default)]
pub struct XAttrEntry {
    /// Length of the attribute name.
    pub name_len: u8,
    /// Attribute name index (e.g. 1 = "user.", 2 = "system.posix_acl_access").
    pub name_index: u8,
    /// Offset of the value within the value area (from end of entry table).
    pub value_offset: u16,
    /// Inode holding the value (0 when value is inline in this block).
    pub value_inum: u32,
    /// Size of the attribute value in bytes.
    pub value_size: u32,
    /// Hash of the attribute name and value.
    pub hash: u32,
}

impl XAttrEntry {
    /// Size of the fixed header (name bytes are *not* included).
    pub const SIZE: usize = 16;

    pub fn read_from(buf: &[u8]) -> Self {
        debug_assert!(buf.len() >= Self::SIZE);
        Self {
            name_len: get_u8(buf, 0),
            name_index: get_u8(buf, 1),
            value_offset: get_u16(buf, 2),
            value_inum: get_u32(buf, 4),
            value_size: get_u32(buf, 8),
            hash: get_u32(buf, 12),
        }
    }

    pub fn write_to(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= Self::SIZE);
        put_u8(buf, 0, self.name_len);
        put_u8(buf, 1, self.name_index);
        put_u16(buf, 2, self.value_offset);
        put_u32(buf, 4, self.value_inum);
        put_u32(buf, 8, self.value_size);
        put_u32(buf, 12, self.hash);
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_roundtrip() {
        let mut sb = SuperBlock::default();
        sb.magic = SUPERBLOCK_MAGIC;
        sb.inodes_count = 1024;
        sb.blocks_count_lo = 4096;
        sb.log_block_size = 2; // 4 KiB blocks
        sb.uuid[0] = 0xDE;
        sb.uuid[15] = 0xAD;
        sb.checksum = 0xCAFE_BABE;

        let mut buf = [0u8; SUPERBLOCK_SIZE];
        sb.write_to(&mut buf);

        let sb2 = SuperBlock::read_from(&buf);
        assert_eq!(sb2.magic, SUPERBLOCK_MAGIC);
        assert_eq!(sb2.inodes_count, 1024);
        assert_eq!(sb2.blocks_count_lo, 4096);
        assert_eq!(sb2.log_block_size, 2);
        assert_eq!(sb2.uuid[0], 0xDE);
        assert_eq!(sb2.uuid[15], 0xAD);
        assert_eq!(sb2.checksum, 0xCAFE_BABE);
    }

    #[test]
    fn group_descriptor_roundtrip() {
        let gd = GroupDescriptor {
            block_bitmap_lo: 100,
            inode_bitmap_lo: 101,
            inode_table_lo: 102,
            free_blocks_count_lo: 500,
            free_inodes_count_lo: 200,
            used_dirs_count_lo: 3,
            flags: bg_flags::INODE_ZEROED,
            exclude_bitmap_lo: 0,
            block_bitmap_csum_lo: 0x1234,
            inode_bitmap_csum_lo: 0x5678,
            itable_unused_lo: 190,
            checksum: 0xABCD,
        };

        let mut buf = [0u8; GroupDescriptor::SIZE];
        gd.write_to(&mut buf);

        let gd2 = GroupDescriptor::read_from(&buf);
        assert_eq!(gd2.block_bitmap_lo, 100);
        assert_eq!(gd2.free_blocks_count_lo, 500);
        assert_eq!(gd2.flags, bg_flags::INODE_ZEROED);
        assert_eq!(gd2.checksum, 0xABCD);
    }

    #[test]
    fn inode_roundtrip() {
        let mut inode = Inode::default();
        inode.mode = file_mode::S_IFREG | 0o644;
        inode.set_file_size(0x1_DEAD_BEEF);
        inode.set_uid(100_000);
        inode.set_gid(200_000);
        inode.links_count = 1;
        inode.extra_isize = EXTRA_ISIZE;
        inode.block[0] = 0xFF;

        let mut buf = [0u8; Inode::SIZE];
        inode.write_to(&mut buf);

        let i2 = Inode::read_from(&buf);
        assert!(i2.is_reg());
        assert!(!i2.is_dir());
        assert!(!i2.is_link());
        assert_eq!(i2.file_size(), 0x1_DEAD_BEEF);
        assert_eq!(i2.uid_full(), 100_000);
        assert_eq!(i2.gid_full(), 200_000);
        assert_eq!(i2.links_count, 1);
        assert_eq!(i2.block[0], 0xFF);
    }

    #[test]
    fn root_inode_has_correct_fields() {
        let root = Inode::root_inode();
        assert!(root.is_dir());
        assert_eq!(root.links_count, 2);
        assert_eq!(root.flags, inode_flags::HUGE_FILE);
        assert_eq!(root.extra_isize, EXTRA_ISIZE);
        // Timestamps should be non-zero (we just called timestamp_now).
        assert_ne!(root.atime, 0);
        assert_ne!(root.ctime, 0);
        assert_ne!(root.mtime, 0);
        assert_ne!(root.crtime, 0);
    }

    #[test]
    fn extent_header_roundtrip() {
        let hdr = ExtentHeader {
            magic: EXTENT_HEADER_MAGIC,
            entries: 3,
            max: 4,
            depth: 0,
            generation: 42,
        };

        let mut buf = [0u8; ExtentHeader::SIZE];
        hdr.write_to(&mut buf);

        let hdr2 = ExtentHeader::read_from(&buf);
        assert_eq!(hdr2.magic, EXTENT_HEADER_MAGIC);
        assert_eq!(hdr2.entries, 3);
        assert_eq!(hdr2.max, 4);
        assert_eq!(hdr2.depth, 0);
        assert_eq!(hdr2.generation, 42);
    }

    #[test]
    fn extent_leaf_roundtrip() {
        let ext = ExtentLeaf {
            block: 0,
            len: 10,
            start_hi: 0x00AB,
            start_lo: 0xCDEF_0123,
        };

        let mut buf = [0u8; ExtentLeaf::SIZE];
        ext.write_to(&mut buf);

        let ext2 = ExtentLeaf::read_from(&buf);
        assert_eq!(ext2.block, 0);
        assert_eq!(ext2.len, 10);
        assert_eq!(ext2.start(), 0x00AB_CDEF_0123);
    }

    #[test]
    fn extent_index_roundtrip() {
        let idx = ExtentIndex {
            block: 1000,
            leaf_lo: 0x1234_5678,
            leaf_hi: 0x00FF,
            unused: 0,
        };

        let mut buf = [0u8; ExtentIndex::SIZE];
        idx.write_to(&mut buf);

        let idx2 = ExtentIndex::read_from(&buf);
        assert_eq!(idx2.block, 1000);
        assert_eq!(idx2.leaf(), 0x00FF_1234_5678);
    }

    #[test]
    fn extent_tail_roundtrip() {
        let tail = ExtentTail {
            checksum: 0xDEAD_BEEF,
        };

        let mut buf = [0u8; ExtentTail::SIZE];
        tail.write_to(&mut buf);

        let tail2 = ExtentTail::read_from(&buf);
        assert_eq!(tail2.checksum, 0xDEAD_BEEF);
    }

    #[test]
    fn directory_entry_roundtrip() {
        let de = DirectoryEntry {
            inode: 42,
            rec_len: 20,
            name_len: 5,
            file_type: FileType::Regular as u8,
        };

        let mut buf = [0u8; DirectoryEntry::SIZE];
        de.write_to(&mut buf);

        let de2 = DirectoryEntry::read_from(&buf);
        assert_eq!(de2.inode, 42);
        assert_eq!(de2.rec_len, 20);
        assert_eq!(de2.name_len, 5);
        assert_eq!(de2.file_type, FileType::Regular as u8);
    }

    #[test]
    fn xattr_entry_roundtrip() {
        let xa = XAttrEntry {
            name_len: 4,
            name_index: 1,
            value_offset: 0x100,
            value_inum: 0,
            value_size: 16,
            hash: 0xABCD_EF01,
        };

        let mut buf = [0u8; XAttrEntry::SIZE];
        xa.write_to(&mut buf);

        let xa2 = XAttrEntry::read_from(&buf);
        assert_eq!(xa2.name_len, 4);
        assert_eq!(xa2.name_index, 1);
        assert_eq!(xa2.value_offset, 0x100);
        assert_eq!(xa2.value_size, 16);
        assert_eq!(xa2.hash, 0xABCD_EF01);
    }

    #[test]
    fn timestamp_now_produces_sane_values() {
        let (lo, extra) = timestamp_now();
        // The low 32 bits of seconds should be well past zero (we are past 1970).
        assert!(lo > 1_000_000_000);
        // The nanosecond part (bits 2..31) should be < 1 billion.
        let nanos = extra >> 2;
        assert!(nanos < 1_000_000_000);
    }

    #[test]
    fn inode_size_consistency() {
        // Make sure our offset constants are self-consistent.
        // The inline xattr area starts right after the 160-byte base.
        assert_eq!(Inode::OFF_INLINE_XATTRS, 160);
        // And the total inode is 256 bytes.
        assert_eq!(Inode::SIZE, 256);
    }

    #[test]
    fn superblock_default_is_all_zeros() {
        let sb = SuperBlock::default();
        let mut buf = [0xFFu8; SUPERBLOCK_SIZE];
        sb.write_to(&mut buf);
        // Every byte should be zero.
        assert!(buf.iter().all(|&b| b == 0));
    }
}
