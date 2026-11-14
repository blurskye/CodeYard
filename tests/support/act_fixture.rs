//! Verification for the pinned `act` executable that the opt-in real-runner tests execute.
//!
//! The OpenSSH and CI worker tests include it directly with `#[path]`; it is deliberately not
//! registered in `support/mod.rs`, so unrelated integration binaries do not compile its tests.
//! `codeyard-testing` cannot own it: that crate depends on both callers. The environment path is
//! canonicalized before it is hashed, and the canonical absolute path is what callers execute.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest fixture the verifier reads. The pinned artifact is tens of MiB; a stale path or a
/// replaced file must not be hashed without a bound.
const MAX_FIXTURE_BYTES: u64 = 128 * 1024 * 1024;

/// `act-0.2.89-codeyard.1` from `build.nix`, verified by SHA-256 on the builder.
const PINNED_ACT_SHA256: &str = "6f40cd31825365e749ab5c78168629f6949fcb06562b40aa8cb5c870f921b2b9";
/// The digest the opt-in tests pinned before this change; it is not the current `build.nix`
/// artifact. It stays accepted so a builder that still carries that approved artifact runs the
/// same tests.
const EARLIER_APPROVED_ACT_SHA256: &str =
    "6be37b104430efc210d5130495bedcff2dc7cd6780a38d88f3d205e7f1185cc1";
const APPROVED_ACT_SHA256: [&str; 2] = [PINNED_ACT_SHA256, EARLIER_APPROVED_ACT_SHA256];

/// Why a path is not the approved fixture.
#[derive(Debug, PartialEq, Eq)]
enum ActFixtureError {
    Missing,
    NotAFile,
    TooLarge { limit: u64 },
    Unreadable(std::io::ErrorKind),
    DigestMismatch { actual: String },
}

impl std::fmt::Display for ActFixtureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str("act fixture does not exist"),
            Self::NotAFile => formatter.write_str("act fixture is not a regular file"),
            Self::TooLarge { limit } => write!(formatter, "act fixture exceeds {limit} bytes"),
            Self::Unreadable(kind) => write!(formatter, "act fixture cannot be read: {kind:?}"),
            Self::DigestMismatch { actual } => {
                write!(
                    formatter,
                    "act fixture SHA-256 {actual} is not an approved digest"
                )
            }
        }
    }
}

/// The verified pinned act named by `GITSERV_TEST_ACT_BINARY`.
///
/// The returned path is canonical and absolute, so a caller that sets `Command::current_dir`
/// cannot resolve the verified path to a different executable.
pub fn pinned_act_fixture_from_env() -> PathBuf {
    let requested = std::env::var_os("GITSERV_TEST_ACT_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("GITSERV_TEST_ACT_BINARY must name the pinned act binary"));
    // Resolve once before hashing so verification and execution name the same absolute path.
    let path = std::fs::canonicalize(&requested)
        .map_err(open_error)
        .unwrap_or_else(|error| panic!("GITSERV_TEST_ACT_BINARY {}: {error}", requested.display()));
    if let Err(error) = verify_fixture(&path, MAX_FIXTURE_BYTES, &APPROVED_ACT_SHA256) {
        panic!(
            "GITSERV_TEST_ACT_BINARY {} is not the pinned act: {error}",
            path.display()
        );
    }
    path
}

fn open_error(error: std::io::Error) -> ActFixtureError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ActFixtureError::Missing,
        kind => ActFixtureError::Unreadable(kind),
    }
}

fn verify_fixture(path: &Path, limit: u64, approved: &[&str]) -> Result<(), ActFixtureError> {
    // Reject a directory, device or FIFO before opening it, then bind the checks to the opened
    // descriptor so a replacement between the calls cannot change what is hashed.
    let metadata = std::fs::metadata(path).map_err(open_error)?;
    if !metadata.is_file() {
        return Err(ActFixtureError::NotAFile);
    }
    if metadata.len() > limit {
        return Err(ActFixtureError::TooLarge { limit });
    }
    let file = std::fs::File::open(path).map_err(open_error)?;
    let metadata = file.metadata().map_err(open_error)?;
    if !metadata.is_file() {
        return Err(ActFixtureError::NotAFile);
    }
    if metadata.len() > limit {
        return Err(ActFixtureError::TooLarge { limit });
    }
    // Check the size again while streaming: the file can grow after the metadata call, and an
    // unbounded read must never run.
    let mut reader = file.take(limit.saturating_add(1));
    let mut buffer = [0u8; 64 * 1024];
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| ActFixtureError::Unreadable(error.kind()))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > limit {
            return Err(ActFixtureError::TooLarge { limit });
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    if approved.contains(&actual.as_str()) {
        Ok(())
    } else {
        Err(ActFixtureError::DigestMismatch { actual })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: u64 = 1024;

    fn write_fixture(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("act");
        std::fs::write(&path, bytes).unwrap();
        (directory, path)
    }

    fn digest(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn approved_digest_passes_and_reads_past_one_buffer() {
        // 70 KiB crosses the 64 KiB read buffer, so the streaming hash decides the digest.
        let bytes = vec![0x5a; 70 * 1024];
        let expected = digest(&bytes);
        let (_directory, path) = write_fixture(&bytes);
        let result = verify_fixture(&path, MAX_FIXTURE_BYTES, &[expected.as_str()]);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn wrong_digest_is_rejected() {
        let (_directory, path) = write_fixture(b"not the pinned act\n");
        let error = verify_fixture(&path, LIMIT, &[PINNED_ACT_SHA256]).unwrap_err();
        assert!(
            matches!(error, ActFixtureError::DigestMismatch { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn input_over_the_limit_is_rejected() {
        let (_directory, path) = write_fixture(b"0123456789");
        let error = verify_fixture(&path, 9, &[]).unwrap_err();
        assert_eq!(error.to_string(), "act fixture exceeds 9 bytes");
    }

    #[test]
    fn missing_input_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("absent");
        let error = verify_fixture(&path, LIMIT, &[]).unwrap_err();
        assert_eq!(error.to_string(), "act fixture does not exist");
    }

    #[test]
    fn directory_input_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let error = verify_fixture(directory.path(), LIMIT, &[]).unwrap_err();
        assert_eq!(error.to_string(), "act fixture is not a regular file");
    }
}
