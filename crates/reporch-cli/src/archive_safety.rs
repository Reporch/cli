use std::io::{Read, Seek};

use anyhow::{Context, Result, ensure};
use zip::ZipArchive;

const MAX_EXPANSION_RATIO: u64 = 100;
const ENTRY_RATIO_SLACK: u64 = 1024 * 1024;
const ARCHIVE_RATIO_SLACK: u64 = 64 * 1024 * 1024;

pub fn validate_zip_resource_budget<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    max_entries: usize,
    max_expanded_bytes: u64,
) -> Result<()> {
    ensure!(
        archive.len() <= max_entries,
        "ZIP archive has too many entries"
    );
    let mut expanded = 0_u64;
    let mut compressed = 0_u64;
    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        let entry_expanded = file.size();
        let entry_compressed = file.compressed_size();
        ensure!(
            entry_expanded <= max_expanded_bytes,
            "ZIP entry exceeds the expanded size budget"
        );
        ensure_expansion_budget(entry_expanded, entry_compressed, ENTRY_RATIO_SLACK).with_context(
            || {
                format!(
                    "ZIP entry has an excessive expansion ratio: {}",
                    file.name()
                )
            },
        )?;
        expanded = expanded
            .checked_add(entry_expanded)
            .context("ZIP expanded size overflow")?;
        compressed = compressed
            .checked_add(entry_compressed)
            .context("ZIP compressed size overflow")?;
        ensure!(
            expanded <= max_expanded_bytes,
            "ZIP archive exceeds its expanded size budget"
        );
        ensure!(
            compressed <= max_expanded_bytes,
            "ZIP archive exceeds its compressed size budget"
        );
    }
    ensure_expansion_budget(expanded, compressed, ARCHIVE_RATIO_SLACK)
        .context("ZIP archive has an excessive aggregate expansion ratio")
}

fn ensure_expansion_budget(expanded: u64, compressed: u64, slack: u64) -> Result<()> {
    let budget = compressed
        .checked_mul(MAX_EXPANSION_RATIO)
        .and_then(|value| value.checked_add(slack))
        .unwrap_or(u64::MAX);
    ensure!(
        expanded <= budget,
        "ZIP expansion ratio exceeds {MAX_EXPANSION_RATIO}:1"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;

    fn archive(contents: &[u8], compression: CompressionMethod) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(&mut output);
        writer
            .start_file(
                "tests/1.in",
                SimpleFileOptions::default().compression_method(compression),
            )
            .unwrap();
        writer.write_all(contents).unwrap();
        writer.finish().unwrap();
        output.into_inner()
    }

    #[test]
    fn rejects_high_ratio_archives_and_accepts_normal_entries() {
        let bomb = archive(&vec![0_u8; 4 * 1024 * 1024], CompressionMethod::Deflated);
        let mut bomb = ZipArchive::new(Cursor::new(bomb)).unwrap();
        assert!(validate_zip_resource_budget(&mut bomb, 10, 8 * 1024 * 1024).is_err());

        let normal = archive(b"normal fixture\n", CompressionMethod::Stored);
        let mut normal = ZipArchive::new(Cursor::new(normal)).unwrap();
        validate_zip_resource_budget(&mut normal, 10, 1024).unwrap();
    }
}
