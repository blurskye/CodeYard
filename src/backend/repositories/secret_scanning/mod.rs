//! Credential detection shared by history scans and push protection.
pub mod jobs;
pub mod patterns;
pub mod push_protection;
mod scanner;

pub use scanner::{scan, secret_types, Detection, MAX_BLOB_BYTES};

pub fn scan_with_patterns(
    path: &str,
    commit: &str,
    bytes: &[u8],
    patterns: &[patterns::Pattern],
    push: bool,
) -> anyhow::Result<Vec<Detection>> {
    let mut found = scan(path, commit, bytes)?;
    if bytes.len() <= MAX_BLOB_BYTES && !bytes.contains(&0) {
        if let Ok(text) = std::str::from_utf8(bytes) {
            found.extend(patterns::scan(patterns, text, push)?);
        }
    }
    anyhow::ensure!(
        found.len() <= scanner::MAX_DETECTIONS,
        "Blob exceeds secret scanning detection budget"
    );
    Ok(found)
}
