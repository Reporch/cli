<p align="center">
  <h1 align="center">arcbox-ext4</h1>
  <p align="center">
    Pure-Rust ext4 filesystem formatter and reader.<br>
    No kernel mount. No FUSE. No C dependencies.
  </p>
</p>

<p align="center">
  <a href="https://crates.io/crates/arcbox-ext4"><img src="https://img.shields.io/crates/v/arcbox-ext4.svg" alt="crates.io"></a>
  <a href="https://docs.rs/arcbox-ext4"><img src="https://docs.rs/arcbox-ext4/badge.svg" alt="docs.rs"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg" alt="License"></a>
</p>

---

`arcbox-ext4` creates and reads ext4 filesystem images entirely in userspace. It is designed for one job: converting OCI container image layers into mountable ext4 block devices on macOS and Linux, without needing `mkfs.ext4`, `libext2fs`, or any Linux tools on the host.

This is the first pure-Rust ext4 mkfs implementation.

## Why

Container runtimes on macOS need to build ext4 root filesystems from OCI image layers. The standard approach requires either shelling out to Linux `mkfs.ext4` (not available on macOS) or linking against C libraries like `lwext4`. This crate does it in pure Rust.

Inspired by [Apple's ContainerizationEXT4](https://github.com/apple/containerization) (the Swift ext4 implementation in Apple's open-source container runtime), then audited line-by-line against it and the ext4 spec.

## Features

| | |
|---|---|
| **Formatter** | Create ext4 images from scratch -- superblock, group descriptors, inode table, bitmaps, extent trees |
| **Reader** | Open existing ext4 images -- path resolution, symlink following, file reading |
| **OCI Unpack** | Stream tar layers directly into ext4 with full OCI whiteout support |
| **Extended Attributes** | Inline (in-inode) and block-level xattrs with name compression |
| **Hard Links** | Correct reference counting with deferred block reclamation |
| **Symlinks** | Fast symlinks (inline, < 60 bytes) and slow symlinks (data blocks) |

## Quick Start

```toml
[dependencies]
arcbox-ext4 = "0.1"
```

### Create an ext4 image

```rust
use std::path::Path;
use arcbox_ext4::{Formatter, constants::{make_mode, file_mode}};

let mut fmt = Formatter::new(Path::new("rootfs.ext4"), 4096, 64 * 1024 * 1024)?;

// Create directories and files.
fmt.create("/etc", make_mode(file_mode::S_IFDIR, 0o755),
    None, None, None, None, None, None)?;
fmt.create("/etc/hostname", make_mode(file_mode::S_IFREG, 0o644),
    None, None, Some(&mut b"arcbox\n".as_slice()), None, None, None)?;

// Create a symlink.
fmt.create("/etc/localtime", make_mode(file_mode::S_IFLNK, 0o777),
    Some("/usr/share/zoneinfo/UTC"), None, None, None, None, None)?;

// Finalize -- writes superblock, group descriptors, bitmaps, inode table.
fmt.close()?;
```

### Read an ext4 image

```rust
use arcbox_ext4::Reader;

let mut reader = Reader::new(std::path::Path::new("rootfs.ext4"))?;

// Check existence, list directories, read files.
assert!(reader.exists("/etc/hostname"));
let entries = reader.list_dir("/etc")?;
let data = reader.read_file("/etc/hostname", 0, None)?;
assert_eq!(&data, b"arcbox\n");
```

### Unpack OCI layers

```rust
use arcbox_ext4::Formatter;

let mut fmt = Formatter::new(path, 4096, 512 * 1024 * 1024)?;

// Apply layers in order. Whiteouts (.wh.* and .wh..wh..opq) are handled.
fmt.unpack_tar(layer1_reader)?;
fmt.unpack_tar(layer2_reader)?;

fmt.close()?;
```

## Architecture

```
                    ┌─────────────┐
  OCI tar layers ──▶│  unpack.rs  │
                    └──────┬──────┘
                           ▼
                    ┌─────────────┐         ┌─────────────┐
    user code ────▶ │ formatter.rs│────────▶│   .ext4     │
                    └─────────────┘  close()│   image     │
                                            └──────┬──────┘
                                                   ▼
                                            ┌─────────────┐
                    user code ────────────▶ │  reader.rs  │
                                            └─────────────┘
```

Internally, the formatter writes data sequentially (files, then directories, then metadata) and computes the final layout at `close()` time:

1. File/symlink data blocks are appended as `create()` is called
2. Directory entries are committed in BFS order (sorted for `e2fsck`)
3. Block group layout is optimized to minimize group count
4. Inode table, block/inode bitmaps, group descriptors, and superblock are written last

## ext4 Feature Flags

| Flag | Status |
|------|--------|
| `extents` | Enabled (extent trees, not legacy block maps) |
| `filetype` | Enabled (directory entries store file type) |
| `flex_bg` | Enabled (flexible block groups) |
| `sparse_super2` | Enabled |
| `huge_file` | Enabled (block-unit `i_blocks` counting) |
| `extra_isize` | Enabled (256-byte inodes with inline xattrs) |
| `ext_attr` | Enabled |
| `has_journal` | Not supported (not needed for container rootfs) |
| `metadata_csum` | Not supported |
| `64bit` | Not supported (32-bit block addresses, max 16 TiB) |

## Limitations

- Block size is fixed at **4096 bytes**
- Maximum file size: **128 GiB**
- Extent tree depth limited to **1** (sufficient for 128 GiB)
- No journal -- images are meant to be built once and mounted read-only (or read-write without crash recovery)

## Testing

133 tests covering:
- Struct serialization round-trips for all on-disk types
- Formatter + Reader end-to-end (files, dirs, symlinks, hardlinks, xattrs)
- OCI two-layer rootfs simulation (Alpine-like)
- Low-level struct validation (superblock fields, group descriptors, bitmaps, inode table)
- Error paths, symlink loops, boundary conditions
- Bug regression tests (hardlink reclaim, symlink classification, block counting)

```sh
cargo test
```

## Acknowledgments

Architecture inspired by Apple's [ContainerizationEXT4](https://github.com/apple/containerization) Swift implementation, then audited line-by-line against it and the [ext4 disk layout specification](https://ext4.wiki.kernel.org/index.php/Ext4_Disk_Layout).

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.
