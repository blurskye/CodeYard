//! The node's key. Its public half is the node's identity in the cluster (the iroh
//! endpoint id). The secret half stays on this machine.
use anyhow::{anyhow, bail, ensure, Context, Result};
use iroh::{PublicKey, SecretKey};
use std::{
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use uuid::Uuid;

/// File inside the state directory that holds a generated key.
const KEY_FILE: &str = "node.key";

#[derive(Clone)]
pub struct NodeKey {
    /// Shared by copies: a node context clones its key, and the key is large.
    secret: Arc<SecretKey>,
}

impl fmt::Debug for NodeKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "NodeKey({})", self.public_hex())
    }
}

impl NodeKey {
    pub fn generate() -> Self {
        Self {
            secret: Arc::new(SecretKey::generate()),
        }
    }

    /// Reads the 64-character hex form that `cluster keygen` prints.
    pub fn parse(text: &str) -> Result<Self> {
        let secret = SecretKey::from_str(&text.trim().to_ascii_lowercase()).map_err(|_| {
            anyhow!("not a valid node key; expected 64 hex characters, like `codeyard cluster keygen` prints")
        })?;
        Ok(Self {
            secret: Arc::new(secret),
        })
    }

    pub fn public_key(&self) -> PublicKey {
        self.secret.public()
    }

    /// Lowercase hex, the form stored in `nodes.public_key`.
    pub fn public_hex(&self) -> String {
        self.secret.public().to_string()
    }

    pub fn secret_hex(&self) -> String {
        hex::encode(self.secret.to_bytes())
    }

    pub(super) fn iroh_secret(&self) -> SecretKey {
        (*self.secret).clone()
    }
}

/// Where a key comes from, in order: `NODE_KEY`, `NODE_KEY_FILE`, then the saved file
/// in the state directory.
enum Source {
    Inline(String),
    File(String),
    Saved,
}

fn source(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Source> {
    let value = |name: &str| {
        lookup(name)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    match (value("NODE_KEY"), value("NODE_KEY_FILE")) {
        (Some(_), Some(_)) => bail!("set NODE_KEY or NODE_KEY_FILE, not both"),
        (Some(text), None) => Ok(Source::Inline(text)),
        (None, Some(path)) => Ok(Source::File(path)),
        (None, None) => Ok(Source::Saved),
    }
}

/// The key for `serve` and `join`: the configured one, or the one saved in the state
/// directory, which is generated on first use.
pub fn load_or_create(state_dir: &Path) -> Result<NodeKey> {
    load(state_dir, &|name| std::env::var(name).ok(), true)?
        .ok_or_else(|| anyhow!("no node key available"))
}

/// Like [`load_or_create`] but never generates a key; `None` if there is none yet.
pub fn load_existing(state_dir: &Path) -> Result<Option<NodeKey>> {
    load(state_dir, &|name| std::env::var(name).ok(), false)
}

fn load(
    state_dir: &Path,
    lookup: &dyn Fn(&str) -> Option<String>,
    create: bool,
) -> Result<Option<NodeKey>> {
    match source(lookup)? {
        Source::Inline(text) => NodeKey::parse(&text).context("NODE_KEY").map(Some),
        Source::File(path) => {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("cannot read NODE_KEY_FILE {path}"))?;
            NodeKey::parse(&text)
                .with_context(|| format!("NODE_KEY_FILE {path}"))
                .map(Some)
        }
        Source::Saved => saved_key(&state_dir.join(KEY_FILE), create),
    }
}

fn saved_key(path: &Path, create: bool) -> Result<Option<NodeKey>> {
    match fs::read_to_string(path) {
        Ok(text) => {
            return NodeKey::parse(&text)
                .with_context(|| format!("key file {}", path.display()))
                .map(Some)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read key file {}", path.display()))
        }
    }
    if !create {
        return Ok(None);
    }
    let key = NodeKey::generate();
    // The key is written to a private temporary file and then linked into place, so
    // it appears complete and never replaces a key another process saved first. The
    // temporary name is random, so a file left by an interrupted start cannot block this one.
    let directory = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    let temporary = write_private(
        directory.join(format!("{KEY_FILE}.{}.tmp", Uuid::new_v4().simple())),
        &format!("{}\n", key.secret_hex()),
    )
    .with_context(|| format!("cannot write a temporary key in {}", directory.display()))?;
    let linked = fs::hard_link(&temporary.0, path);
    drop(temporary);
    match linked {
        Ok(()) => Ok(Some(key)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => saved_key(path, false),
        Err(error) => {
            Err(error).with_context(|| format!("cannot save the node key to {}", path.display()))
        }
    }
}

/// A file that is removed when dropped, so no path out leaves a copy of the key behind.
struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Creates a new file readable only by its owner and writes `contents` to it. The file is
/// removed when the returned guard is dropped.
fn write_private(path: PathBuf, contents: &str) -> io::Result<TemporaryFile> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    // The file is ours from here on, so any later error removes it.
    let temporary = TemporaryFile(path);
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(temporary)
}

/// Splits `public key` text of the form `<64 hex>` back into the key type, for
/// callers that hold it as a string.
pub fn parse_public_key(text: &str) -> Result<PublicKey> {
    let text = text.trim();
    ensure!(
        crate::backend::crypto::is_hex_token(text, 64),
        "a public key is 64 hex characters"
    );
    PublicKey::from_str(&text.to_ascii_lowercase())
        .map_err(|_| anyhow!("that is not a valid public key"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the environment that owns its entries.
    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn key_text_round_trips_and_rejects_garbage() {
        let key = NodeKey::generate();
        let again = NodeKey::parse(&format!(" {}\n", key.secret_hex())).unwrap();
        assert_eq!(key.public_hex(), again.public_hex());
        assert_eq!(key.public_hex().len(), 64);
        assert!(NodeKey::parse("").is_err());
        assert!(NodeKey::parse("abcd").is_err());
        assert!(NodeKey::parse(&"z".repeat(64)).is_err());
    }

    #[test]
    fn errors_never_repeat_the_secret() {
        let secret = format!("{}!", "ab".repeat(32));
        let error = NodeKey::parse(&secret).unwrap_err().to_string();
        assert!(!error.contains(&secret));
        let debug = format!("{:?}", NodeKey::generate());
        assert!(debug.starts_with("NodeKey(") && debug.len() < 80);
    }

    #[test]
    fn configured_keys_win_over_the_saved_file_and_cannot_be_ambiguous() {
        let directory = tempfile::tempdir().unwrap();
        let key = NodeKey::generate();
        let hex = key.secret_hex();
        let inline = lookup(&[("NODE_KEY", &hex)]);
        let loaded = load(directory.path(), &inline, true).unwrap().unwrap();
        assert_eq!(loaded.public_hex(), key.public_hex());
        // A configured key never writes a file.
        assert!(!directory.path().join(KEY_FILE).exists());

        let file = directory.path().join("secret");
        fs::write(&file, format!("{hex}\n")).unwrap();
        let path = file.to_str().unwrap();
        let from_file = lookup(&[("NODE_KEY_FILE", path)]);
        let loaded = load(directory.path(), &from_file, true).unwrap().unwrap();
        assert_eq!(loaded.public_hex(), key.public_hex());

        let both = lookup(&[("NODE_KEY", &hex), ("NODE_KEY_FILE", path)]);
        assert!(load(directory.path(), &both, true).is_err());
        let missing = lookup(&[("NODE_KEY_FILE", "/nonexistent/key")]);
        assert!(load(directory.path(), &missing, true).is_err());
        // Empty values count as unset, as compose files pass them.
        let empty = lookup(&[("NODE_KEY", ""), ("NODE_KEY_FILE", "  ")]);
        assert!(load(directory.path(), &empty, false).unwrap().is_none());
    }

    #[test]
    fn generated_key_is_saved_privately_and_reused() {
        let directory = tempfile::tempdir().unwrap();
        let nothing = lookup(&[]);
        assert!(load(directory.path(), &nothing, false).unwrap().is_none());
        let first = load(directory.path(), &nothing, true).unwrap().unwrap();
        let again = load(directory.path(), &nothing, true).unwrap().unwrap();
        assert_eq!(first.public_hex(), again.public_hex());
        let saved = directory.path().join(KEY_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&saved).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // Only the key file is left behind, no temporary copies.
        let names: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        // A damaged file is an error, not a reason to make a new identity.
        fs::write(&saved, "not a key").unwrap();
        assert!(load(directory.path(), &nothing, true).is_err());
    }

    #[test]
    fn a_leftover_temporary_file_does_not_block_a_new_key() {
        let directory = tempfile::tempdir().unwrap();
        // The name an earlier start gave its temporary file: the process id, which is 1 in a container.
        fs::write(directory.path().join("node.tmp-1"), "interrupted").unwrap();
        let nothing = lookup(&[]);
        let created = load(directory.path(), &nothing, true).unwrap().unwrap();
        let again = load(directory.path(), &nothing, false).unwrap().unwrap();
        assert_eq!(created.public_hex(), again.public_hex());
    }

    #[cfg(unix)]
    #[test]
    fn a_key_that_cannot_be_published_leaves_no_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        // A dangling link holds the key's name, so the key cannot be linked there.
        std::os::unix::fs::symlink(
            directory.path().join("missing"),
            directory.path().join(KEY_FILE),
        )
        .unwrap();
        // No key is published, so the call yields no key or an error. Only the files matter here.
        let _ = load(directory.path(), &lookup(&[]), true);
        let names: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from(KEY_FILE)]);
    }

    #[test]
    fn public_keys_must_be_full_hex() {
        let key = NodeKey::generate().public_hex();
        assert!(parse_public_key(&key).is_ok());
        assert!(parse_public_key(&key[..60]).is_err());
        assert!(parse_public_key(&key.to_uppercase()).is_ok());
        assert!(parse_public_key("").is_err());
    }
}
