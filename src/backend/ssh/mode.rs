//! Startup-only SSH transport selection. Detection never edits or reloads sshd.
use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const DEFAULT_SOCKET: &str = "/run/codeyard/ssh.sock";
pub const DEFAULT_MANIFEST: &str = "/etc/codeyard/openssh.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum SshMode {
    #[default]
    Auto,
    Openssh,
    Builtin,
    Disabled,
}
impl SshMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Openssh => "openssh",
            Self::Builtin => "builtin",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detection {
    Active,
    Inactive,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub mode: SshMode,
    pub setup_required: bool,
    pub git_user: String,
    pub socket: PathBuf,
    pub builtin_port: u16,
}

static SELECTED: OnceLock<Selection> = OnceLock::new();
pub fn selected() -> Option<&'static Selection> {
    SELECTED.get()
}
pub fn publish(selection: Selection) -> Result<()> {
    SELECTED
        .set(selection)
        .map_err(|_| anyhow::anyhow!("SSH transport was already selected"))
}

pub fn choose(requested: SshMode, detected: Detection) -> SshMode {
    match (requested, detected) {
        (SshMode::Auto, Detection::Active) => SshMode::Openssh,
        (SshMode::Auto, Detection::Inactive) => SshMode::Builtin,
        (SshMode::Auto, Detection::Unknown) => SshMode::Disabled,
        (mode, _) => mode,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Installation {
    schema: u32,
    git_user: String,
    service_user: String,
    socket: PathBuf,
    serve_command: PathBuf,
    binary: PathBuf,
}

pub fn valid_user(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || *b == b'_')
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

fn valid_path(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some_and(|s| {
            !s.ends_with('/')
                && !s.contains("//")
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
                && !s.split('/').any(|part| part == "." || part == "..")
        })
}

fn parse_installation(bytes: &[u8], socket: &Path, git_user: &str) -> Result<()> {
    let value: Installation =
        serde_json::from_slice(bytes).context("Invalid OpenSSH installation manifest")?;
    if value.schema != 1
        || value.git_user != git_user
        || !valid_user(&value.git_user)
        || !valid_user(&value.service_user)
        || value.socket != socket
        || !valid_path(&value.socket)
        || !valid_path(&value.serve_command)
        || !valid_path(&value.binary)
    {
        bail!("OpenSSH installation manifest does not match the configured user/socket");
    }
    Ok(())
}

fn installed(manifest: &Path, socket: &Path, git_user: &str) -> Result<bool> {
    let file = match fs::File::open(manifest) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("Read OpenSSH installation manifest"),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 {
        bail!("OpenSSH installation manifest must be a small regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!("OpenSSH installation manifest must be root-owned and not group/world writable");
        }
    }
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 {
        bail!("OpenSSH installation manifest is too large");
    }
    parse_installation(&bytes, socket, git_user)?;
    // This records an installer-validated configuration, not a live sshd health check.
    Ok(true)
}

pub fn select(
    requested: SshMode,
    socket: PathBuf,
    git_user: String,
    manifest: &Path,
    builtin_port: u16,
) -> Result<Selection> {
    if !valid_user(&git_user) || !valid_path(&socket) || socket.as_os_str().len() > 107 {
        bail!("SSH_GIT_USER and SSH_SOCKET_PATH must be a valid Unix user and safe absolute path");
    }
    let detection = if requested == SshMode::Auto {
        detect()
    } else {
        Detection::Unknown
    };
    let mode = choose(requested, detection);
    if requested == SshMode::Auto && mode == SshMode::Builtin && builtin_port == 22 {
        bail!("Auto mode will not bind port 22; use host OpenSSH or a non-22 SSH_ADDR");
    }
    let setup_required = match mode {
        SshMode::Openssh => !installed(manifest, &socket, &git_user)?,
        SshMode::Disabled => requested == SshMode::Auto,
        _ => false,
    };
    Ok(Selection {
        mode,
        setup_required,
        git_user,
        socket,
        builtin_port,
    })
}

/// Host evidence only: no reliance on SSH_CONNECTION, installed binaries or port probes.
/// Container PID namespaces cannot establish whether the host runs OpenSSH.
pub fn detect() -> Detection {
    if Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
        || std::env::var_os("container").is_some()
        || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
        || fs::read_to_string("/proc/1/cgroup").is_ok_and(|s| {
            ["docker", "kubepods", "libpod", "lxc", "containerd"]
                .iter()
                .any(|name| s.contains(name))
        })
        || fs::read_to_string("/run/systemd/container").is_ok_and(|s| !s.trim().is_empty())
    {
        return Detection::Unknown;
    }
    for binary in [
        "/usr/bin/systemctl",
        "/bin/systemctl",
        // NixOS keeps system binaries here.
        "/run/current-system/sw/bin/systemctl",
    ] {
        if !Path::new(binary).is_file() {
            continue;
        }
        if let Ok(mut child) = Command::new(binary)
            .args([
                "is-active",
                "--quiet",
                "sshd.service",
                "ssh.service",
                "sshd.socket",
                "ssh.socket",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) if status.success() => return Detection::Active,
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        break;
    }
    let Ok(entries) = fs::read_dir("/proc") else {
        return Detection::Unknown;
    };
    // hidepid may omit other users' processes entirely, not just deny reads.
    let mut denied = fs::read_to_string("/proc/mounts").is_ok_and(|s| {
        s.lines()
            .any(|line| line.contains(" /proc ") && line.contains("hidepid="))
    });
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        match fs::read_to_string(entry.path().join("comm")) {
            Ok(name) if name.trim() == "sshd" => return Detection::Active,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
            _ => {}
        }
    }
    if denied {
        Detection::Unknown
    } else {
        Detection::Inactive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn active_host_wins_auto_and_uncertainty_never_opens_a_port() {
        assert_eq!(choose(SshMode::Auto, Detection::Active), SshMode::Openssh);
        assert_eq!(choose(SshMode::Auto, Detection::Inactive), SshMode::Builtin);
        assert_eq!(choose(SshMode::Auto, Detection::Unknown), SshMode::Disabled);
        for mode in [SshMode::Openssh, SshMode::Builtin, SshMode::Disabled] {
            for detection in [Detection::Active, Detection::Inactive, Detection::Unknown] {
                assert_eq!(choose(mode, detection), mode);
            }
        }
    }
    #[test]
    fn manifest_must_match_the_actual_gateway_configuration() {
        let mut value = serde_json::json!({"schema":1,"git_user":"git","service_user":"gitserv","socket":DEFAULT_SOCKET,"serve_command":"/usr/local/libexec/codeyard/serve","binary":"/usr/local/bin/codeyard"});
        let check = |value: &serde_json::Value| {
            parse_installation(
                &serde_json::to_vec(value).unwrap(),
                Path::new(DEFAULT_SOCKET),
                "git",
            )
        };
        assert!(check(&value).is_ok());
        value["git_user"] = "other".into();
        assert!(check(&value).is_err());
        value["git_user"] = "git".into();
        value["socket"] = "/tmp/other.sock".into();
        assert!(check(&value).is_err());
        value["socket"] = DEFAULT_SOCKET.into();
        value["serve_command"] = "/bin/sh;bad".into();
        assert!(check(&value).is_err());
    }
    #[test]
    fn unsafe_names_paths_and_unknown_configuration_are_rejected() {
        for user in [
            "",
            "-root",
            "1git",
            "GIT",
            "git\nroot",
            "git;id",
            "git user",
        ] {
            assert!(!valid_user(user));
        }
        for path in [
            "relative.sock",
            "/",
            "/tmp//ssh.sock",
            "/tmp/ssh.sock/",
            "/tmp/../ssh.sock",
            "/tmp/./ssh.sock",
            "/tmp/socket name",
            "/tmp/x\n",
        ] {
            assert!(!valid_path(Path::new(path)));
        }
        assert!(parse_installation(b"{}", Path::new(DEFAULT_SOCKET), "git").is_err());
    }
}
