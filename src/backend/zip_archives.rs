//! Opening untrusted ZIP archives. The `zip` reader reserves and parses as many central
//! directory entries as the end record it picks advertises, and it tries every end record
//! signature in the file, from the back, until one works. So each record the reader could
//! accept is found first, as the reader would, and the archive is refused if more than one
//! non-empty record could be used or if one advertises more entries than the caller accepts. A signature that
//! merely occurs inside an entry is not such a record.
use std::io::Cursor;
use zip::read::{ArchiveOffset, Config};

const END: &[u8; 4] = b"PK\x05\x06";
const ZIP64_END: &[u8; 4] = b"PK\x06\x06";
const ZIP64_LOCATOR: &[u8; 4] = b"PK\x06\x07";
const CENTRAL_ENTRY: &[u8; 4] = b"PK\x01\x02";
/// The reader reads each candidate's comment, up to 64 KiB, before it can reject it.
const MAX_END_SIGNATURES: usize = 1024;

pub fn open<B: AsRef<[u8]>>(
    bytes: B,
    max_entries: usize,
) -> anyhow::Result<zip::ZipArchive<Cursor<B>>> {
    let raw = bytes.as_ref();
    // A little-endian field, if the bytes hold it.
    let field = |offset: usize, width: usize| -> Option<u64> {
        let bytes = raw.get(offset..offset.checked_add(width)?)?;
        Some(bytes.iter().rev().fold(0, |value, byte| (value << 8) | u64::from(*byte)))
    };
    let magic_at = |offset: u64, magic: &[u8; 4]| {
        usize::try_from(offset).ok().and_then(|offset| raw.get(offset..offset.checked_add(4)?))
            == Some(magic.as_slice())
    };
    let (mut signatures, mut usable) = (0usize, 0usize);
    let mut admit = |on_disk: u64, total: u64| {
        let entries = on_disk.max(total);
        anyhow::ensure!(
            entries <= max_entries as u64,
            "The ZIP archive has more than {max_entries} entries"
        );
        // An empty record costs the reader nothing, such as an empty ZIP stored as an entry.
        if entries > 0 {
            usable += 1;
            anyhow::ensure!(usable == 1, "The ZIP archive has more than one usable end record");
        }
        Ok(())
    };
    for end in (0..raw.len().saturating_sub(3)).filter(|&offset| magic_at(offset as u64, END)) {
        // The reader skips a record that is cut short or whose comment runs past the end.
        let (Some(on_disk), Some(total), Some(directory), Some(comment)) =
            (field(end + 8, 2), field(end + 10, 2), field(end + 16, 4), field(end + 20, 2))
        else {
            continue;
        };
        if end as u64 + 22 + comment > raw.len() as u64 {
            continue;
        }
        signatures += 1;
        anyhow::ensure!(signatures <= MAX_END_SIGNATURES, "The ZIP archive has too many end records");
        let locator = end.checked_sub(20).filter(|&locator| magic_at(locator as u64, ZIP64_LOCATOR));
        if let Some(locator) = locator.filter(|_| total == 0xFFFF || directory == 0xFFFF_FFFF) {
            // The reader then reads only ZIP64 records: the one the locator names, at offset 0,
            // and any others between it and the locator.
            let first = field(locator + 8, 8).unwrap_or(u64::MAX);
            if first < locator as u64 && magic_at(first, ZIP64_END) {
                for record in (first as usize..locator).filter(|&offset| magic_at(offset as u64, ZIP64_END)) {
                    if let (Some(on_disk), Some(total)) = (field(record + 24, 8), field(record + 32, 8)) {
                        admit(on_disk, total)?;
                    }
                }
            }
        } else if total == 0 || (directory < end as u64 && magic_at(directory, CENTRAL_ENTRY)) {
            // An empty directory is accepted as is; otherwise its first entry must start where
            // the record says, since the archive starts at offset 0.
            admit(on_disk, total)?;
        }
    }
    let config = Config {
        archive_offset: ArchiveOffset::Known(0),
    };
    Ok(zip::ZipArchive::with_config(config, Cursor::new(bytes))?)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    fn archive(entries: usize) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for index in 0..entries {
            writer
                .start_file(format!("{index}.txt"), zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"x").unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn entry_count_is_bounded_before_reading() {
        assert_eq!(super::open(archive(3), 3).unwrap().len(), 3);
        assert!(super::open(archive(4), 3).is_err());
        assert!(super::open(b"not a zip".to_vec(), 3).is_err());
        let mut forged = archive(1);
        let footer = forged.windows(4).rposition(|bytes| bytes == b"PK\x05\x06").unwrap();
        forged[footer + 10..footer + 12].copy_from_slice(&60_000u16.to_le_bytes());
        assert!(super::open(forged, 3).is_err());
    }
}
