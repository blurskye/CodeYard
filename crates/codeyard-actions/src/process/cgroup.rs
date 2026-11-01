//! Worker/job-owned cgroup v2 lifetime, independent of process-group leaders.
use anyhow::{bail, ensure, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

pub struct Cgroup {
    path: PathBuf,
    #[cfg(target_os = "linux")]
    procs: std::fs::File,
    #[cfg(target_os = "linux")]
    kill: std::fs::File,
    #[cfg(target_os = "linux")]
    events: std::fs::File,
}

impl Cgroup {
    pub fn current_root() -> Result<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            use std::io::Read;
            let mut text = String::new();
            std::fs::File::open("/proc/self/cgroup")?
                .take(64 * 1024)
                .read_to_string(&mut text)?;
            let relative = text
                .lines()
                .find_map(|line| line.strip_prefix("0::/"))
                .context("A unified cgroup v2 hierarchy is required")?;
            ensure!(
                Path::new(relative)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
                "Invalid current cgroup path"
            );
            let root = Path::new("/sys/fs/cgroup").join(relative);
            check_root(&root)?;
            Ok(root)
        }
        #[cfg(not(target_os = "linux"))]
        bail!("Process-tree containment requires Linux cgroup v2")
    }

    pub fn create(root: &Path, name: &str) -> Result<Self> {
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name != "."
                && name != ".."
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
            "Invalid cgroup name"
        );
        check_root(root)?;
        let path = root.join(name);
        std::fs::create_dir(&path).context("Unable to create a delegated execution cgroup")?;
        Self::open(&path)
    }

    pub fn temporary(root: &Path, prefix: &str) -> Result<Self> {
        ensure!(
            !prefix.is_empty()
                && prefix.len() <= 64
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "Invalid cgroup prefix"
        );
        check_root(root)?;
        let path = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(root)
            .context("Unable to create a delegated execution cgroup")?
            .keep();
        Self::open(&path)
    }

    pub fn open(path: &Path) -> Result<Self> {
        check_root(path)?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let open = |name: &str, write: bool| -> Result<std::fs::File> {
                Ok(std::fs::OpenOptions::new()
                    .read(!write)
                    .write(write)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(path.join(name))?)
            };
            Ok(Self {
                path: path.to_owned(),
                procs: open("cgroup.procs", true)?,
                kill: open("cgroup.kill", true)?,
                events: open("cgroup.events", false)?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        bail!("Process-tree containment requires Linux cgroup v2")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Migration occurs in the child before exec and before it can create descendants.
    pub fn attach(&self, command: &mut tokio::process::Command) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let procs = self.procs.try_clone()?;
            // SAFETY: the child hook performs only an async-signal-safe write; the owned fd
            // stays open through the hook and is close-on-exec.
            unsafe {
                command.pre_exec(move || {
                    let written = libc::write(procs.as_raw_fd(), b"0".as_ptr().cast(), 1);
                    if written == 1 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = command;
            bail!("Process-tree containment requires Linux cgroup v2")
        }
    }

    pub fn kill(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::FileExt;
            ensure!(
                self.kill.write_at(b"1", 0)? == 1,
                "Unable to terminate execution cgroup"
            );
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        bail!("Process-tree containment requires Linux cgroup v2")
    }

    pub fn populated(&self) -> Result<bool> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::FileExt;
            let mut bytes = [0u8; 4096];
            let length = self.events.read_at(&mut bytes, 0)?;
            ensure!(length < bytes.len(), "Oversized cgroup state");
            let text = std::str::from_utf8(&bytes[..length])?;
            match text
                .lines()
                .find_map(|line| line.strip_prefix("populated "))
            {
                Some("0") => Ok(false),
                Some("1") => Ok(true),
                _ => bail!("Invalid cgroup state"),
            }
        }
        #[cfg(not(target_os = "linux"))]
        bail!("Process-tree containment requires Linux cgroup v2")
    }

    /// Give the runner its shutdown grace, then kill every independently grouped descendant.
    pub async fn stop(
        &self,
        child: &mut tokio::process::Child,
        group: &mut super::ProcessGroup,
        grace: Duration,
    ) -> Result<()> {
        if child.id().is_some() && !grace.is_zero() {
            group.terminate()?;
            // The cgroup now owns cleanup. Never signal a numeric group ID after reaping.
            group.disarm();
            let _ = tokio::time::timeout(grace, child.wait()).await;
        } else {
            group.disarm();
        }
        self.kill()?;
        tokio::time::timeout(super::KILL_GRACE, child.wait())
            .await
            .context("Runner did not stop after cgroup termination")??;
        Ok(())
    }

    pub async fn cleanup(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        match self.kill() {
            Ok(()) => {}
            Err(error) if removed(&error) => return Ok(()),
            Err(error) => return Err(error),
        }
        loop {
            match self.populated() {
                Ok(false) => break,
                Ok(true) => {}
                Err(error) if removed(&error) => return Ok(()),
                Err(error) => return Err(error),
            }
            ensure!(
                Instant::now() < deadline,
                "Execution cgroup did not become empty"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || remove_empty_tree(&path, deadline)).await??;
        Ok(())
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

fn check_root(path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: statfs writes to initialized-size output; directory owns a valid fd.
        ensure!(
            unsafe { libc::fstatfs(directory.as_raw_fd(), stat.as_mut_ptr()) } == 0,
            "Unable to inspect cgroup filesystem"
        );
        // SAFETY: fstatfs succeeded, so it filled `stat`.
        ensure!(
            unsafe { stat.assume_init() }.f_type as u64 == 0x6367_7270,
            "Execution containment requires cgroup v2"
        );
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        bail!("Process-tree containment requires Linux cgroup v2")
    }
}

fn removed(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.raw_os_error() == Some(libc::ENODEV))
}

fn remove_empty_tree(path: &Path, deadline: Instant) -> Result<()> {
    let mut pending = vec![(path.to_owned(), 0usize, false)];
    let mut visited = 0usize;
    while let Some((directory, depth, exiting)) = pending.pop() {
        ensure!(
            Instant::now() < deadline && depth <= 128 && visited <= 100_000,
            "Cgroup cleanup exceeded its traversal budget"
        );
        if exiting {
            std::fs::remove_dir(directory)?;
            continue;
        }
        visited += 1;
        check_root(&directory)?;
        pending.push((directory.clone(), depth, true));
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                ensure!(pending.len() < 100_000, "Too many child cgroups");
                pending.push((entry.path(), depth + 1, false));
            }
        }
    }
    Ok(())
}
