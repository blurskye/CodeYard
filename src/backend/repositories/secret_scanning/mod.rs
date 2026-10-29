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
    scan_with_patterns_until(path, commit, bytes, patterns, push, std::time::Instant::now() + std::time::Duration::from_secs(10))
}

pub(crate) fn scan_with_patterns_until(
    path: &str,
    commit: &str,
    bytes: &[u8],
    patterns: &[patterns::Pattern],
    push: bool,
    deadline: std::time::Instant,
) -> anyhow::Result<Vec<Detection>> {
    anyhow::ensure!(std::time::Instant::now() < deadline, "Secret scanning exceeded its time budget");
    let mut found = scan(path, commit, bytes)?;
    if bytes.len() <= MAX_BLOB_BYTES && !bytes.contains(&0) {
        if let Ok(text) = std::str::from_utf8(bytes) {
            found.extend(patterns::scan(patterns, text, push, deadline)?);
        }
    }
    anyhow::ensure!(
        found.len() <= scanner::MAX_DETECTIONS
            && found.iter().map(|detection| detection.secret.len()).sum::<usize>() <= scanner::MAX_MATCHED_BYTES,
        "Blob exceeds secret scanning detection budget"
    );
    Ok(found)
}
