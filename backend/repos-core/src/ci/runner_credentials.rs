//! Runner credentials use cluster pairing unchanged; removal has its own hash domain.
use crate::context::NodeContext;
use anyhow::Context;
use anyhow::Result;
use codeyard_cluster_core::pairing::JoinTicket;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

#[derive(Serialize, Deserialize)]
pub struct JitConfig {
    pub ticket: String,
    pub node_name: String,
    /// Relative within the state's dedicated runner-workspaces directory.
    pub work_folder: String,
}
impl JitConfig {
    pub fn decode(encoded: &str) -> Result<Self> {
        use base64::Engine;
        anyhow::ensure!(encoded.len() <= 16384, "runner configuration is too large");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("invalid runner configuration")?;
        let config: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid runner configuration"))?;
        codeyard_cluster_core::pairing::normalize_name(&config.node_name)?;
        JoinTicket::decode(&config.ticket)
            .map_err(|_| anyhow::anyhow!("invalid runner pairing credential"))?;
        anyhow::ensure!(
            valid_work_folder(&config.work_folder),
            "invalid runner work folder"
        );
        Ok(config)
    }
}
pub fn jit_from_env() -> Result<Option<JitConfig>> {
    match std::env::var("CODEYARD_RUNNER_JIT_CONFIG") {
        Ok(value) if !value.trim().is_empty() => Ok(Some(JitConfig::decode(&value)?)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => anyhow::bail!("CODEYARD_RUNNER_JIT_CONFIG is not valid UTF-8"),
    }
}
#[derive(Serialize, Deserialize)]
struct SavedRunner {
    work_folder: String,
}
pub fn valid_work_folder(folder: &str) -> bool {
    !folder.is_empty()
        && !folder.chars().any(char::is_control)
        && folder.len() <= 255
        && std::path::Path::new(folder)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}
pub fn save_work_folder(state: &std::path::Path, folder: &str) -> Result<()> {
    use std::io::Write;
    anyhow::ensure!(valid_work_folder(folder), "invalid runner work folder");
    let _state = codeyard_support::fs::lock_state_dir(state)?;
    let previous = work_root(state)?;
    if previous != state.join("runner-workspaces").join(folder) {
        match std::fs::read_dir(previous.join("ci-containers")) {
            Ok(mut records) => anyhow::ensure!(
                records.next().transpose()?.is_none(),
                "recover previous CI workspaces and containers before changing the work folder"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    // A new file renamed over the old one, so a crash leaves the old settings or the new ones.
    let mut file = tempfile::NamedTempFile::new_in(state)?;
    file.write_all(&serde_json::to_vec(&SavedRunner {
        work_folder: folder.to_owned(),
    })?)?;
    file.as_file().sync_all()?;
    file.persist(state.join("runner-settings.json"))
        .map_err(|error| error.error)?;
    std::fs::File::open(state)?.sync_all()?;
    Ok(())
}
pub fn work_root(state: &std::path::Path) -> Result<std::path::PathBuf> {
    if let Some(root) = std::env::var_os("CI_WORK_ROOT") {
        let root = std::path::PathBuf::from(root);
        anyhow::ensure!(root.is_absolute(), "CI_WORK_ROOT must be absolute");
        return Ok(root);
    }
    let bytes = match std::fs::read(state.join("runner-settings.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(state.to_owned()),
        Err(error) => return Err(error.into()),
    };
    let saved: SavedRunner = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        valid_work_folder(&saved.work_folder),
        "invalid saved runner work folder"
    );
    Ok(state.join("runner-workspaces").join(saved.work_folder))
}
pub fn new_remove_token() -> String {
    format!(
        "codeyard-remove-v1.{}",
        codeyard_support::crypto::random_hex()
    )
}
pub fn remove_hash(token: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"codeyard runner removal v1\0");
    hash.update(token.as_bytes());
    hex::encode(hash.finalize())
}
pub async fn ticket(
    pool: &codeyard_db_core::DbPool,
    context: Option<&NodeContext>,
) -> Result<JoinTicket> {
    let core = context.context("runner registration requires a serving PostgreSQL cluster core")?;
    let node = codeyard_db_cluster::cluster::get_node_by_id(pool, core.node_id)
        .await?
        .filter(|n| n.status == "active")
        .context("runner core is unavailable")?;
    let address = serde_json::from_str(
        node.addr_hint
            .as_deref()
            .context("runner core address is unavailable")?,
    )?;
    JoinTicket::new(core.key.public_key(), address)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jit_credentials_reuse_pairing_and_validate_local_paths() {
        use base64::Engine;
        let key = codeyard_support::node_identity::NodeKey::generate();
        let ticket = JoinTicket::new(
            key.public_key(),
            codeyard_cluster_core::transport::NodeAddress {
                tcp: vec!["127.0.0.1:1".parse().unwrap()],
                ..Default::default()
            },
        )
        .unwrap();
        let config = JitConfig {
            ticket: ticket.encode(),
            node_name: "runner-42".into(),
            work_folder: "_work".into(),
        };
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&config).unwrap());
        let decoded = JitConfig::decode(&encoded).unwrap();
        assert_eq!(
            JoinTicket::decode(&decoded.ticket).unwrap().secret_hash(),
            ticket.secret_hash()
        );
        let state = tempfile::tempdir().unwrap();
        save_work_folder(state.path(), &decoded.work_folder).unwrap();
        assert_eq!(
            work_root(state.path()).unwrap(),
            state.path().join("runner-workspaces/_work")
        );
        // A valid folder must not collide with the state's own settings file.
        save_work_folder(state.path(), "runner-settings.json").unwrap();
        let work = work_root(state.path()).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        assert_eq!(
            work,
            state.path().join("runner-workspaces/runner-settings.json")
        );
        for folder in [
            "",
            "..",
            "../escape",
            "/absolute",
            "bad\0folder",
            "line\nbreak",
        ] {
            assert!(save_work_folder(state.path(), folder).is_err());
        }
        assert!(JitConfig::decode("invalid").is_err());
        let malformed =
            base64::engine::general_purpose::STANDARD.encode(br#""private-credential-marker""#);
        let error = JitConfig::decode(&malformed).err().unwrap();
        assert!(!format!("{error:#}").contains("private-credential-marker"));
        assert_ne!(remove_hash(&config.ticket), ticket.secret_hash());
    }
    #[test]
    fn runner_settings_never_follow_a_symlink() {
        let state = tempfile::tempdir().unwrap();
        let target = state.path().join("unrelated");
        std::fs::write(&target, r#"{"work_folder":"old"}"#).unwrap();
        std::os::unix::fs::symlink(&target, state.path().join("runner-settings.json")).unwrap();
        // The new settings are renamed over the link, so its target is never written.
        save_work_folder(state.path(), "_work").unwrap();
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            r#"{"work_folder":"old"}"#
        );
        let saved = state.path().join("runner-settings.json");
        assert!(std::fs::symlink_metadata(&saved).unwrap().is_file());
        assert_eq!(
            work_root(state.path()).unwrap(),
            state.path().join("runner-workspaces/_work")
        );
    }
}
