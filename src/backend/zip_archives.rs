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
    let possible = bytes
        .as_ref()
        .windows(CENTRAL_ENTRY.len())
        .filter(|window| *window == CENTRAL_ENTRY)
        .count();
    anyhow::ensure!(
        possible <= max_entries,
        "The ZIP archive has more than {max_entries} entries"
    );
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
    }
}
