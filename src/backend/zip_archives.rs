//! Opening untrusted ZIP archives. The `zip` reader builds metadata for every central
//! directory entry before it returns, so a small archive of many tiny entries can cost far
//! more memory than its size. Each caller names the most entries it accepts, and an archive
//! that could hold more is refused before the reader runs.
use std::io::Cursor;

/// Every central directory entry starts with this signature, so the number of times it
/// occurs in the bytes bounds how many entries any reader can parse from them.
const CENTRAL_ENTRY: &[u8; 4] = b"PK\x01\x02";

pub fn open<B: AsRef<[u8]>>(
    bytes: B,
    max_entries: usize,
) -> anyhow::Result<zip::ZipArchive<Cursor<B>>> {
    let raw = bytes.as_ref();
    let mut possible = 0usize;
    for (offset, signature) in raw.windows(4).enumerate() {
        if signature == CENTRAL_ENTRY {
            possible += 1;
            anyhow::ensure!(possible <= max_entries, "The ZIP archive has more than {max_entries} entries");
        } else if signature == b"PK\x05\x06" {
            // The reader reserves from the advertised count before checking entries. Bound
            // every candidate footer, including one embedded in an archive comment.
            for start in [8, 10] {
                let count = raw.get(offset + start..offset + start + 2).ok_or_else(|| anyhow::anyhow!("Truncated ZIP footer"))?;
                anyhow::ensure!(u16::from_le_bytes(count.try_into()?) as usize <= max_entries, "ZIP advertised entry count exceeds its limit");
            }
        } else if signature == b"PK\x06\x06" {
            for start in [24, 32] {
                let count = raw.get(offset + start..offset + start + 8).ok_or_else(|| anyhow::anyhow!("Truncated ZIP64 footer"))?;
                anyhow::ensure!(u64::from_le_bytes(count.try_into()?) <= max_entries as u64, "ZIP64 advertised entry count exceeds its limit");
            }
        }
    }
    Ok(zip::ZipArchive::new(Cursor::new(bytes))?)
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
