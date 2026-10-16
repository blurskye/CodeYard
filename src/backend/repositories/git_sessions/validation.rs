use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Component, Path};

pub(crate) const MAX_HEADER: usize = 512 * 1024;
pub(crate) const MAX_HOOK_BYTES: usize = 256 * 1024;
pub(crate) const MAX_UPDATES: usize = 256;
pub(crate) const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GitCommand {
    pub owner: String,
    pub repo: String,
    pub write: bool,
}

pub(crate) fn parse_command(command: &str) -> Result<GitCommand> {
    // Room for the longest owner and repository names that registration accepts.
    ensure!(command.len() <= 2048, "invalid Git command");
    let (service, path) = command
        .split_once(' ')
        .ok_or_else(|| anyhow::anyhow!("invalid Git command"))?;
    let write = match service {
        "git-upload-pack" => false,
        "git-receive-pack" => true,
        _ => bail!("only git-upload-pack and git-receive-pack are supported"),
    };
    let path = if path.starts_with('\'') {
        path.strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .ok_or_else(|| anyhow::anyhow!("invalid Git path"))?
    } else {
        path
    };
    let path = path.strip_prefix('/').unwrap_or(path);
    let (owner, repo) = path
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid Git path"))?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    // Account and organization names have no dots; repository names may.
    ensure!(
        valid_component(owner) && !owner.contains('.') && valid_component(repo),
        "invalid Git path"
    );
    Ok(GitCommand {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        write,
    })
}

/// Account and repository names are database keys here, not file names (disk paths come
/// from the repository's UUID), so this accepts what registration and repository creation
/// accept: letters and digits in any script, `-`, `_` and `.`, up to 255 characters.
fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value.chars().count() <= 255
        && !matches!(value, "." | "..")
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub(crate) fn shell_safe_path(path: &Path) -> Result<&str> {
    let value = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path must be UTF-8"))?;
    ensure!(
        path.is_absolute() && value.len() <= 4096 && value != "/",
        "absolute path required"
    );
    ensure!(
        value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-')),
        "path must be shell-safe (ASCII letters, digits, slash, dot, underscore, hyphen)"
    );
    ensure!(
        !value[1..]
            .split('/')
            .any(|part| matches!(part, "." | ".." | "")),
        "path must not contain empty or traversal components"
    );
    ensure!(
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "invalid path"
    );
    Ok(value)
}

pub(crate) fn protocol(value: Option<&str>) -> Result<Option<&str>> {
    match value {
        None | Some("") => Ok(None),
        Some("version=1") => Ok(Some("version=1")),
        Some("version=2") => Ok(Some("version=2")),
        _ => bail!("unsupported GIT_PROTOCOL"),
    }
}

/// Parse and re-encode the wire key, never echo caller input or a DB comment/options.
pub(crate) fn canonical_key(key_type: &str, blob: &str) -> Result<String> {
    crate::backend::db::users::ssh_keys::canonical_key(key_type, blob)
}

pub(crate) fn canonical_db_key(public_key: &str, key_type: &str) -> Result<String> {
    let key = crate::backend::db::users::ssh_keys::parse_public_key(public_key)?;
    ensure!(
        key.algorithm().to_string() == key_type,
        "stored SSH key type mismatch"
    );
    Ok(key.to_openssh()?)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RefUpdate {
    pub old: String,
    pub new: String,
    pub reference: String,
}

pub(crate) fn validate_updates(updates: &[RefUpdate]) -> Result<()> {
    ensure!(
        !updates.is_empty() && updates.len() <= MAX_UPDATES,
        "invalid number of ref updates"
    );
    let mut names = HashSet::new();
    let mut bytes = 0usize;
    for update in updates {
        bytes += update.old.len() + update.new.len() + update.reference.len() + 3;
        ensure!(
            sha(&update.old)
                && sha(&update.new)
                && (update.old != ZERO_SHA || update.new != ZERO_SHA),
            "invalid SHA"
        );
        ensure!(
            crate::backend::git::is_canonical_ref(&update.reference),
            "invalid ref"
        );
        ensure!(names.insert(&update.reference), "duplicate ref update");
    }
    ensure!(bytes <= MAX_HOOK_BYTES, "hook input too large");
    Ok(())
}

pub(crate) fn parse_updates(input: &[u8]) -> Result<Vec<RefUpdate>> {
    ensure!(
        input.len() <= MAX_HOOK_BYTES && input.ends_with(b"\n"),
        "invalid hook input"
    );
    let text = std::str::from_utf8(input)?;
    let mut updates = Vec::new();
    for line in text.split_terminator('\n') {
        let fields: Vec<_> = line.split(' ').collect();
        ensure!(
            fields.len() == 3 && updates.len() < MAX_UPDATES,
            "invalid hook input"
        );
        updates.push(RefUpdate {
            old: fields[0].to_owned(),
            new: fields[1].to_owned(),
            reference: fields[2].to_owned(),
        });
    }
    validate_updates(&updates)?;
    Ok(updates)
}

fn sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_a_single_service_and_repository_not_a_shell() {
        for command in [
            "git-upload-pack 'alice/private.git'",
            "git-upload-pack /alice/private",
            "git-receive-pack '/alice/repo.git'",
            // One transport suffix is stripped, so a repository may itself end in `.git`.
            "git-upload-pack alice/project.git.git",
            "git-upload-pack 'équipe/功能.git'",
            "git-upload-pack alice/a..b",
        ] {
            assert!(parse_command(command).is_ok(), "{command}");
        }
        // The longest name registration accepts: 255 characters, 510 bytes here.
        assert!(parse_command(&format!("git-upload-pack '{}/repo.git'", "é".repeat(255))).is_ok());
        assert!(parse_command(&format!("git-upload-pack '{}/repo.git'", "é".repeat(256))).is_err());
        for command in [
            "git-upload-pack //alice/repo",
            "git-upload-pack alice/../repo",
            "git-upload-pack alice/.git",
            "git-upload-pack .git/repo",
            "git-upload-pack 'alice/repo' x",
            "git-upload-pack \"alice/repo\"",
            "git-upload-pack alice/repo;id",
            "git-upload-pack $(id)/repo",
            "git-upload-pack alice/repo\n",
            "git-upload-pack  alice/repo",
            "git-upload-pack alice/repo/extra",
            "sh alice/repo",
        ] {
            assert!(parse_command(command).is_err(), "{command}");
        }
    }

    #[test]
    fn canonical_key_never_contains_options_comments_or_injected_lines() {
        // Wire encoding: string ssh-ed25519, string 32-byte public key.
        use base64::Engine;
        let mut wire = Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
        wire.extend_from_slice(&[7; 32]);
        let blob = base64::engine::general_purpose::STANDARD.encode(wire);
        let canonical = canonical_key("ssh-ed25519", &blob).unwrap();
        assert_eq!(
            canonical_db_key(&format!("{canonical} user comment"), "ssh-ed25519").unwrap(),
            canonical
        );
        assert!(canonical_key("ssh-rsa", &blob).is_err());
        assert!(canonical_key("ssh-ed25519", &format!("{blob}\n")).is_err());
        assert!(canonical_db_key(&format!("{canonical}\ncommand=bad"), "ssh-ed25519").is_err());
        assert!(canonical_db_key(&format!("command=bad {canonical}"), "ssh-ed25519").is_err());
    }

    #[test]
    fn paths_protocol_and_hook_updates_fail_closed() {
        assert!(shell_safe_path(Path::new("/opt/codeyard")).is_ok());
        for path in [
            "relative",
            "/opt/../bin/helper",
            "/opt/a b",
            "/opt/helper;id",
            "/opt//helper",
            "/opt/./helper",
        ] {
            assert!(shell_safe_path(Path::new(path)).is_err());
        }
        assert_eq!(protocol(Some("version=2")).unwrap(), Some("version=2"));
        assert!(protocol(Some("version=2:evil=x")).is_err());
        let line = format!("{} {} refs/heads/main\n", ZERO_SHA, "1".repeat(40));
        assert_eq!(parse_updates(line.as_bytes()).unwrap().len(), 1);
        for reference in [
            "refs/heads/release/stable",
            "refs/tags/v1",
            "refs/heads/équipe/功能",
        ] {
            assert_eq!(
                parse_updates(format!("{} {} {reference}\n", ZERO_SHA, "1".repeat(40)).as_bytes())
                    .unwrap()[0]
                    .reference,
                reference
            );
        }
        assert!(parse_updates(format!("{line}{line}").as_bytes()).is_err());
        for reference in [
            "refs/heads/release//stable",
            "refs/tags/release//stable",
            "refs/heads//main",
            "refs/heads/../main",
            "refs/heads/.git",
            "refs/heads/main.lock",
            "refs/heads/a b",
            "refs/replace/x",
        ] {
            assert!(parse_updates(
                format!("{} {} {reference}\n", ZERO_SHA, "1".repeat(40)).as_bytes()
            )
            .is_err());
        }
        assert!(parse_updates(
            format!("{} {} refs/heads/main", ZERO_SHA, "1".repeat(40)).as_bytes()
        )
        .is_err());
    }
}
