//! Follows actions/runner's `src/Runner.Worker/Handlers/ScriptHandler.cs` for PowerShell steps.
//! QEMU is a Codeyard backend, not an upstream runner feature.
//!
//! Parse `vm:/path/base.qcow2[,cpus=N,memory=NG]` as [`Platform`], then call
//! [`JobVm::start`]. Copy the workspace to [`WORKSPACE`] with [`JobVm::copy_in`],
//! execute steps with [`JobVm::run`], copy it back with [`JobVm::copy_out`], and
//! call [`JobVm::stop`]. Output callbacks receive lines as they arrive, including
//! a final unterminated line. Dropping the VM (also on task cancellation) kills
//! QEMU and removes its overlay, seed ISO, SSH key and known-hosts file.
//!
//! Hosts need QEMU, qemu-img, OpenSSH 9+ (SFTP scp), ssh-keygen and xorriso.
//! Base images must implement the CODEYARD seed-CD contract in
//! `scripts/windows-base-image/README.md`. Only loopback SSH is forwarded.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout, timeout_at, Instant};

pub const WORKSPACE: &str = "C:/codeyard/workspace";
const GUEST_TEMP: &str = "C:/codeyard/tmp";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    pub base_image: PathBuf,
    pub cpus: u16,
    pub memory_mib: u64,
}

impl FromStr for Platform {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let value = value
            .strip_prefix("vm:")
            .context("Expected 'vm:<path>[,cpus=N,memory=NG]'")?;
        let mut fields = value.split(',');
        let path = fields.next().unwrap_or_default();
        ensure!(!path.is_empty(), "VM base image path must not be empty");
        let mut platform = Self {
            base_image: path.into(),
            cpus: 2,
            memory_mib: 4096,
        };
        let (mut seen_cpus, mut seen_memory) = (false, false);
        for field in fields {
            if let Some(value) = field.strip_prefix("cpus=") {
                ensure!(!seen_cpus, "Duplicate VM option 'cpus'");
                seen_cpus = true;
                platform.cpus = value
                    .parse()
                    .context("VM cpus must be a positive integer")?;
            } else if let Some(value) = field.strip_prefix("memory=") {
                ensure!(!seen_memory, "Duplicate VM option 'memory'");
                seen_memory = true;
                let gib = value
                    .strip_suffix('G')
                    .context("VM memory must be an integer followed by 'G'")?;
                platform.memory_mib = gib
                    .parse::<u64>()
                    .context("VM memory must be an integer followed by 'G'")?
                    .checked_mul(1024)
                    .context("VM memory is too large")?;
            } else {
                bail!("Unrecognized VM option: '{field}'");
            }
        }
        platform.validate()?;
        Ok(platform)
    }
}

impl Platform {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.base_image.as_os_str().is_empty(),
            "VM base image path must not be empty"
        );
        ensure!(self.cpus > 0, "VM cpus must be positive");
        ensure!(self.memory_mib > 0, "VM memory must be positive");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub allow_tcg: bool,
    pub boot_timeout: Duration,
    pub shutdown_grace: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            allow_tcg: false,
            boot_timeout: Duration::from_secs(300),
            shutdown_grace: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Owns all per-job files. Construct this before the first cancellable operation.
struct JobFiles(tempfile::TempDir);

impl JobFiles {
    fn create() -> Result<Self> {
        Ok(Self(crate::fs::private_tempdir("codeyard-vm-", None)?))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }
}

pub struct JobVm {
    // Drop QEMU before unlinking its files. kill_on_drop also covers cancelled waits.
    child: Child,
    files: JobFiles,
    port: u16,
    shutdown_grace: Duration,
    next_script: u64,
}

impl JobVm {
    pub async fn start(platform: &Platform, options: &Options) -> Result<Self> {
        platform.validate()?;
        let base = platform
            .base_image
            .canonicalize()
            .context("Opening VM base image")?;
        ensure!(base.is_file(), "VM base image is not a file");
        let kvm = kvm_usable();
        ensure!(
            kvm || options.allow_tcg,
            "KVM is not usable; explicitly allow TCG to run a VM without KVM"
        );
        if !kvm {
            eprintln!("##[warning]KVM is unavailable: running the VM with TCG software emulation; Windows jobs will be very slow");
        }
        let files = JobFiles::create()?;
        let deadline = Instant::now() + options.boot_timeout;
        let boot = async {
            checked(
                Command::new("qemu-img")
                    .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
                    .arg(&base)
                    .arg(files.path("overlay.qcow2")),
            )
            .await?;
            checked(
                Command::new("ssh-keygen")
                    .args(["-q", "-t", "ed25519", "-N", "", "-C", "codeyard-job", "-f"])
                    .arg(files.path("id_ed25519")),
            )
            .await?;
            fs::create_dir(files.path("seed"))?;
            fs::copy(
                files.path("id_ed25519.pub"),
                files.path("seed/authorized_keys"),
            )?;
            checked(
                Command::new("xorriso")
                    .args([
                        "-as", "mkisofs", "-quiet", "-J", "-r", "-V", "CODEYARD", "-o",
                    ])
                    .arg(files.path("seed.iso"))
                    .arg(files.path("seed")),
            )
            .await?;
            // QEMU cannot inherit a prebound hostfwd socket. Keep the reservation until spawn;
            // if another process wins the small bind race, QEMU exits and boot reports it.
            let reservation = TcpListener::bind(("127.0.0.1", 0))?;
            let port = reservation.local_addr()?.port();
            let log = fs::File::create(files.path("qemu.log"))?;
            let mut command = Command::new("qemu-system-x86_64");
            command
                .args(qemu_args(platform, &files, port, kvm)?)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(log)
                .kill_on_drop(true);
            drop(reservation);
            let child = command.spawn().context("Starting qemu-system-x86_64")?;
            Ok::<_, anyhow::Error>((child, port))
        };
        let (child, port) = timeout_at(deadline, boot)
            .await
            .context("VM boot preparation timed out")??;
        let mut vm = Self {
            child,
            files,
            port,
            shutdown_grace: options.shutdown_grace,
            next_script: 0,
        };
        let result = timeout_at(deadline, vm.wait_for_ssh()).await;
        match result {
            Ok(Ok(())) => Ok(vm),
            result => {
                let log = fs::read_to_string(vm.files.path("qemu.log")).unwrap_or_default();
                match result {
                    Ok(Err(error)) => Err(error).context(format!("VM failed to boot: {log}")),
                    _ => bail!("Timed out waiting for the VM's OpenSSH server: {log}"),
                }
            }
        }
    }

    async fn wait_for_ssh(&mut self) -> Result<()> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!("QEMU exited with {status}");
            }
            let mut command = self.ssh();
            command.arg("pwsh -NoLogo -NoProfile -NonInteractive -Command \"exit 0\"");
            if let Ok(Ok(status)) = timeout(
                Duration::from_secs(10),
                command.stdout(Stdio::null()).stderr(Stdio::null()).status(),
            )
            .await
            {
                if status.success() {
                    return Ok(());
                }
            }
            sleep(Duration::from_secs(1)).await;
        }
    }

    fn transport(&self, program: &str, port_flag: &str) -> Command {
        let mut command = Command::new(program);
        command
            .args([port_flag, &self.port.to_string(), "-i"])
            .arg(self.files.path("id_ed25519"))
            .args([
                "-F",
                "/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ConnectionAttempts=1",
                "-o",
                "ServerAliveInterval=5",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
            ])
            .arg(format!(
                "UserKnownHostsFile=\"{}\"",
                self.files.path("known_hosts").to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"")
            ))
            .args([
                "-o",
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "LogLevel=ERROR",
            ])
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    fn ssh(&self) -> Command {
        let mut command = self.transport("ssh", "-p");
        command.args(["-T", "runner@127.0.0.1"]);
        command
    }

    /// Runs a PowerShell step, using the upstream error and LASTEXITCODE conventions.
    /// A nonzero guest exit code is returned, not turned into an infrastructure error.
    /// Cancelling a step kills its SSH client; drop the JobVm as well to stop guest work.
    pub async fn run<F>(
        &mut self,
        script: &str,
        env: &BTreeMap<String, String>,
        working_directory: &str,
        mut output: F,
    ) -> Result<i32>
    where
        F: FnMut(Stream, &str),
    {
        let directory = guest_path(working_directory)?;
        let script = step_script(script, env, &directory)?;
        let id = self.next_script;
        self.next_script += 1;
        let local = self.files.path(&format!("step-{id}.ps1"));
        let remote = format!("{GUEST_TEMP}/step-{id}.ps1");
        fs::write(&local, script)?;
        let mut copy = self.transport("scp", "-P");
        checked(copy.arg(&local).arg(format!("runner@127.0.0.1:{remote}"))).await?;
        let mut child = self
            .ssh()
            .arg(format!(
                "pwsh -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {remote}"
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Running PowerShell over SSH")?;
        // The runner's bounded reader keeps a guest that never ends a line from growing this
        // process's memory.
        let (out_sender, mut stdout) = tokio::sync::mpsc::channel(64);
        let (err_sender, mut stderr) = tokio::sync::mpsc::channel(64);
        tokio::spawn(crate::runner::read_lines(
            child.stdout.take().context("SSH stdout is unavailable")?,
            out_sender,
        ));
        tokio::spawn(crate::runner::read_lines(
            child.stderr.take().context("SSH stderr is unavailable")?,
            err_sender,
        ));
        let (mut out_done, mut err_done) = (false, false);
        while !out_done || !err_done {
            tokio::select! {
                line = stdout.recv(), if !out_done => match line {
                    Some(line) => output(Stream::Stdout, &line),
                    None => out_done = true,
                },
                line = stderr.recv(), if !err_done => match line {
                    Some(line) => output(Stream::Stderr, &line),
                    None => err_done = true,
                },
            }
        }
        let status = child.wait().await?;
        let _ = fs::remove_file(local);
        let code = status.code().context("SSH was terminated by a signal")?;
        // OpenSSH also uses 255 for transport failures, which are streamed on stderr.
        Ok(code)
    }

    /// Copies directory contents, including dotfiles, to an existing guest directory.
    /// The provisioned image creates WORKSPACE and GUEST_TEMP before SSH is ready.
    pub async fn copy_in(&self, local: &Path, guest_directory: &str) -> Result<()> {
        let local = local
            .canonicalize()
            .context("Opening workspace to copy into VM")?;
        ensure!(local.is_dir(), "Workspace is not a directory");
        let guest = guest_path(guest_directory)?;
        let mut command = self.transport("scp", "-P");
        checked(
            command
                .arg("-r")
                .arg(local.join("."))
                .arg(format!("runner@127.0.0.1:{guest}/")),
        )
        .await
    }

    /// Copies guest directory contents back. This is a merge, not a deletion sync.
    pub async fn copy_out(&self, guest_directory: &str, local: &Path) -> Result<()> {
        let guest = guest_path(guest_directory)?;
        fs::create_dir_all(local)?;
        let local = local.canonicalize()?;
        let mut command = self.transport("scp", "-P");
        checked(
            command
                .arg("-r")
                .arg(format!("runner@127.0.0.1:{guest}/."))
                .arg(local),
        )
        .await
    }

    /// Requests ACPI shutdown, then kills QEMU if the grace period expires.
    pub async fn stop(mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            if let Some(mut monitor) = self.child.stdin.take() {
                let _ = timeout(
                    Duration::from_secs(1),
                    monitor.write_all(b"system_powerdown\n"),
                )
                .await;
            }
            match timeout(self.shutdown_grace, self.child.wait()).await {
                Ok(result) => {
                    result.context("Waiting for QEMU shutdown")?;
                }
                Err(_) => self
                    .child
                    .kill()
                    .await
                    .context("Stopping QEMU after shutdown grace period")?,
            }
        }
        Ok(())
    }
}

impl Drop for JobVm {
    fn drop(&mut self) {
        // Start the kill synchronously, before JobFiles removes the open overlay.
        let _ = self.child.start_kill();
    }
}

fn kvm_usable() -> bool {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

fn qemu_args(platform: &Platform, files: &JobFiles, port: u16, kvm: bool) -> Result<Vec<String>> {
    let overlay = qemu_path(&files.path("overlay.qcow2"))?;
    let seed = qemu_path(&files.path("seed.iso"))?;
    Ok(vec![
        "-name".into(),
        "codeyard-job".into(),
        "-machine".into(),
        format!("q35,accel={}", if kvm { "kvm" } else { "tcg" }),
        "-cpu".into(),
        if kvm { "host" } else { "max" }.into(),
        "-smp".into(),
        platform.cpus.to_string(),
        "-m".into(),
        platform.memory_mib.to_string(),
        "-drive".into(),
        format!("file={overlay},format=qcow2,if=ide,index=0"),
        "-drive".into(),
        format!("file={seed},format=raw,media=cdrom,readonly=on,if=ide,index=2"),
        "-netdev".into(),
        format!("user,id=net0,hostfwd=tcp:127.0.0.1:{port}-:22"),
        "-device".into(),
        "e1000e,netdev=net0".into(),
        "-display".into(),
        "none".into(),
        "-serial".into(),
        "none".into(),
        "-monitor".into(),
        "stdio".into(),
        "-no-reboot".into(),
    ])
}

fn qemu_path(path: &Path) -> Result<String> {
    Ok(path
        .to_str()
        .context("QEMU paths must be UTF-8")?
        .replace(',', ",,"))
}

fn guest_path(path: &str) -> Result<String> {
    let path = path.replace('\\', "/");
    let bytes = path.as_bytes();
    ensure!(
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1..3] == *b":/",
        "Guest directory must be an absolute Windows path"
    );
    ensure!(
        !path
            .chars()
            .any(|c| c.is_control() || "*?[]\"<>|".contains(c)),
        "Invalid guest directory"
    );
    // A drive root keeps its slash: `C:` alone means that drive's current directory.
    let trimmed = path.trim_end_matches('/');
    Ok(if trimmed.len() == 2 {
        format!("{trimmed}/")
    } else {
        trimmed.to_owned()
    })
}

fn ps_literal(value: &str) -> String {
    let mut literal = String::from("'");
    for ch in value.chars() {
        literal.push(ch);
        if matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') { literal.push(ch); }
    }
    literal.push('\'');
    literal
}

fn step_script(script: &str, env: &BTreeMap<String, String>, directory: &str) -> Result<String> {
    let mut result = String::from("$ErrorActionPreference = 'Stop'\n");
    for (key, value) in env {
        ensure!(
            !key.is_empty() && !key.contains(['=', '\0']) && !value.contains('\0'),
            "Invalid environment variable: '{key}'"
        );
        result.push_str(&format!(
            "[Environment]::SetEnvironmentVariable({}, {}, 'Process')\n",
            ps_literal(key),
            ps_literal(value)
        ));
    }
    result.push_str(&format!(
        "Set-Location -LiteralPath {}\n",
        ps_literal(directory)
    ));
    result.push_str(script);
    result
        .push_str("\nif (Test-Path -LiteralPath variable:\\LASTEXITCODE) { exit $LASTEXITCODE }\n");
    Ok(result)
}

async fn checked(command: &mut Command) -> Result<()> {
    command.kill_on_drop(true).stdin(Stdio::null());
    let description = format!("{command:?}");
    let output = crate::runner::capture_output(command, Duration::from_secs(300))
        .await
        .with_context(|| format!("Starting {description}"))?;
    ensure!(
        output.status.success(),
        "{description} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_platform() {
        let default: Platform = "vm:/images/windows.qcow2".parse().unwrap();
        assert_eq!(default.base_image, Path::new("/images/windows.qcow2"));
        assert_eq!((default.cpus, default.memory_mib), (2, 4096));
        let custom: Platform = "vm:relative disk.qcow2,memory=8G,cpus=4".parse().unwrap();
        assert_eq!((custom.cpus, custom.memory_mib), (4, 8192));
    }

    #[test]
    fn rejects_invalid_platforms() {
        for input in [
            "",
            "docker:x",
            "vm:",
            "vm:x,",
            "vm:x,cpus=0",
            "vm:x,cpus=-1",
            "vm:x,cpus=65536",
            "vm:x,memory=0G",
            "vm:x,memory=4",
            "vm:x,memory=1.5G",
            "vm:x,memory=18446744073709551615G",
            "vm:x,cpus=2,cpus=3",
            "vm:x,memory=2G,memory=3G",
            "vm:x,gpu=1",
        ] {
            assert!(input.parse::<Platform>().is_err(), "{input}");
        }
    }

    #[test]
    fn qemu_uses_overlay_loopback_and_requested_resources() {
        let platform: Platform = "vm:/read-only/base.qcow2,cpus=4,memory=8G".parse().unwrap();
        let files = JobFiles::create().unwrap();
        let args = qemu_args(&platform, &files, 22022, true).unwrap();
        assert!(args.windows(2).any(|a| a == ["-smp", "4"]));
        assert!(args.windows(2).any(|a| a == ["-m", "8192"]));
        assert!(args.contains(&"q35,accel=kvm".into()));
        assert!(args.contains(&"host".into()));
        assert!(args.contains(&"user,id=net0,hostfwd=tcp:127.0.0.1:22022-:22".into()));
        assert!(args
            .iter()
            .any(|a| a.contains("overlay.qcow2,format=qcow2")));
        assert!(args
            .iter()
            .any(|a| a.contains("seed.iso,format=raw,media=cdrom,readonly=on")));
        assert!(!args.iter().any(|a| a.contains("base.qcow2")));
        let tcg = qemu_args(&platform, &files, 22023, false).unwrap();
        assert!(tcg.contains(&"q35,accel=tcg".into()));
        assert!(tcg.contains(&"max".into()));
        assert_eq!(qemu_path(Path::new("/tmp/a,b")).unwrap(), "/tmp/a,,b");
    }

    #[test]
    fn cleanup_on_error_removes_overlay_and_secrets() {
        let mut directory = PathBuf::new();
        let result: Result<()> = (|| {
            let files = JobFiles::create()?;
            directory = files.0.path().to_path_buf();
            for name in ["overlay.qcow2", "id_ed25519", "known_hosts", "seed.iso"] {
                fs::write(files.path(name), "secret")?;
            }
            bail!("boot failed")
        })();
        assert!(result.is_err());
        assert!(!directory.exists());
    }

    #[tokio::test]
    async fn cancellation_kills_qemu_and_removes_files() {
        let files = JobFiles::create().unwrap();
        let directory = files.0.path().to_path_buf();
        fs::write(files.path("overlay.qcow2"), "overlay").unwrap();
        let child = Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let vm = JobVm {
            child,
            files,
            port: 22,
            shutdown_grace: Duration::from_millis(10),
            next_script: 0,
        };
        let task = tokio::spawn(async move {
            let _vm = vm;
            std::future::pending::<()>().await;
        });
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!directory.exists());
        // Linux exposes the process until the runtime has reaped the killed child.
        #[cfg(target_os = "linux")]
        {
            timeout(Duration::from_secs(5), async {
                while Path::new(&format!("/proc/{pid}")).exists() {
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn stop_requests_acpi_and_cleans_files() {
        let files = JobFiles::create().unwrap();
        let directory = files.0.path().to_path_buf();
        let result = tempfile::tempdir().unwrap();
        let received = result.path().join("monitor-command");
        let child = Command::new("sh")
            .args([
                "-c",
                "read -r command; printf '%s' \"$command\" > \"$1\"",
                "sh",
            ])
            .arg(&received)
            .stdin(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let vm = JobVm {
            child,
            files,
            port: 22,
            shutdown_grace: Duration::from_secs(5),
            next_script: 0,
        };
        vm.stop().await.unwrap();
        assert_eq!(fs::read_to_string(received).unwrap(), "system_powerdown");
        assert!(!directory.exists());
    }

    #[tokio::test]
    async fn stop_kills_an_unresponsive_guest() {
        let files = JobFiles::create().unwrap();
        let directory = files.0.path().to_path_buf();
        let child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let vm = JobVm {
            child,
            files,
            port: 22,
            shutdown_grace: Duration::from_millis(10),
            next_script: 0,
        };
        timeout(Duration::from_secs(5), vm.stop())
            .await
            .unwrap()
            .unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn quotes_step_environment_and_directory() {
        let env = BTreeMap::from([("weird'name".into(), "a'’\n$()".into())]);
        let script = step_script("git status", &env, "C:/some ' dir").unwrap();
        assert!(script.contains("'weird''name', 'a''’’\n$()', 'Process'"));
        assert!(script.contains("Set-Location -LiteralPath 'C:/some '' dir'"));
        assert!(script.starts_with("$ErrorActionPreference = 'Stop'\n"));
        assert!(script.ends_with(
            "if (Test-Path -LiteralPath variable:\\LASTEXITCODE) { exit $LASTEXITCODE }\n"
        ));
        assert!(step_script(
            "",
            &BTreeMap::from([("bad=name".into(), "x".into())]),
            WORKSPACE
        )
        .is_err());
    }

    #[test]
    fn validates_guest_paths() {
        assert_eq!(guest_path("C:\\codeyard\\workspace").unwrap(), WORKSPACE);
        for path in ["relative", "~/x", "C:/x*", "C:/x\n", "C:/x?", "C:/x[0]"] {
            assert!(guest_path(path).is_err(), "{path}");
        }
    }

    #[tokio::test]
    #[ignore = "needs kvm and a windows image"]
    async fn windows_boot_step_and_copy() {
        let base = std::env::var("CODEYARD_WINDOWS_IMAGE").expect("set CODEYARD_WINDOWS_IMAGE");
        let platform: Platform = format!("vm:{base}").parse().unwrap();
        let mut vm = JobVm::start(&platform, &Options::default()).await.unwrap();
        let input = tempfile::tempdir().unwrap();
        fs::write(input.path().join(".hidden"), "input").unwrap();
        vm.copy_in(input.path(), WORKSPACE).await.unwrap();
        let mut output = Vec::new();
        let env = BTreeMap::from([("TEST_VALUE".into(), "hello ' world".into())]);
        let exit = vm.run("Get-Content .hidden\nWrite-Output $env:TEST_VALUE\nSet-Content result.txt 'copied back'\nexit 7", &env, WORKSPACE,
            |stream, line| output.push((stream, line.to_owned()))).await.unwrap();
        assert_eq!(exit, 7);
        assert!(output.contains(&(Stream::Stdout, "input".into())));
        assert!(output.contains(&(Stream::Stdout, "hello ' world".into())));
        let destination = tempfile::tempdir().unwrap();
        vm.copy_out(WORKSPACE, destination.path()).await.unwrap();
        assert!(fs::read_to_string(destination.path().join("result.txt"))
            .unwrap()
            .contains("copied back"));
        let files = vm.files.0.path().to_path_buf();
        vm.stop().await.unwrap();
        assert!(!files.exists());
    }
}
