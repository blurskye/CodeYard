//! Shared bounded subprocess output framing. Oversized lines are omitted, never truncated:
//! retaining a prefix could expose part of a credential that whole-value masking cannot match.
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};
mod cgroup;
pub use cgroup::Cgroup;

pub const OMITTED_LINE: &str = "Warning: An oversized output line was omitted.";
pub const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(unix)]
const INT: i32 = libc::SIGINT;
#[cfg(unix)]
const TERM: i32 = libc::SIGTERM;
#[cfg(unix)]
const KILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const INT: i32 = 0;
#[cfg(not(unix))]
const TERM: i32 = 0;
#[cfg(not(unix))]
const KILL: i32 = 0;

/// One owner for a subprocess group. Callers explicitly choose a TERM grace; zero means
/// immediate KILL. Successful cleanup disarms before output can wait on a sink.
pub struct ProcessGroup {
    id: Option<u32>,
    grace: std::time::Duration,
}

impl ProcessGroup {
    pub fn new(id: Option<u32>, grace: std::time::Duration) -> Self { Self { id, grace } }
    pub fn pid(&self) -> Option<u32> { self.id }
    pub fn disarm(&mut self) { self.id = None; }
    pub fn kill(&mut self) { kill_group(self.id.take()); }
    pub fn terminate(&self) -> std::io::Result<()> {
        if let Some(id) = self.id { signal_group(id, TERM)?; }
        Ok(())
    }

    /// Sends SIGINT, as a terminal's Ctrl+C does, and waits up to `grace` for the group to end.
    /// True when it ended.
    pub async fn interrupt(&mut self, child: &mut tokio::process::Child, grace: std::time::Duration) -> std::io::Result<bool> {
        let Some(id) = self.id else { return Ok(true); };
        signal_group(id, INT)?;
        if wait_for_group(child, id, grace).await? { self.disarm(); return Ok(true); }
        Ok(false)
    }

    pub async fn stop(&mut self, child: &mut tokio::process::Child) -> std::io::Result<()> {
        let Some(id) = self.id else { return Ok(()); };
        if !group_alive(id)? {
            let _ = child.try_wait();
            self.disarm();
            return Ok(());
        }
        if !self.grace.is_zero() {
            signal_group(id, TERM)?;
            if wait_for_group(child, id, self.grace).await? { self.disarm(); return Ok(()); }
        }
        signal_group(id, KILL)?;
        if !wait_for_group(child, id, KILL_GRACE).await? {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Process group did not stop in time"));
        }
        self.disarm();
        Ok(())
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else { return; };
        if self.grace.is_zero() { kill_group(Some(id)); return; }
        let _ = signal_group(id, TERM);
        let grace = self.grace;
        if std::thread::Builder::new().name("process-group-cleanup".into()).spawn(move || {
            let deadline = std::time::Instant::now() + grace;
            while std::time::Instant::now() < deadline {
                if matches!(group_alive(id), Ok(false)) { return; }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            kill_group(Some(id));
        }).is_err() { kill_group(Some(id)); }
    }
}

pub fn kill_group(id: Option<u32>) {
    if let Some(id) = id { let _ = signal_group(id, KILL); }
}

fn signal_group(id: u32, signal: i32) -> std::io::Result<()> {
    #[cfg(unix)] {
        let id = i32::try_from(id).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        // SAFETY: negative PID addresses only the group created for this child.
        if unsafe { libc::kill(-id, signal) } == 0 { return Ok(()); }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) { return Err(error); }
    }
    #[cfg(not(unix))] let _ = (id, signal);
    Ok(())
}

fn group_alive(id: u32) -> std::io::Result<bool> {
    #[cfg(unix)] {
        let id = i32::try_from(id).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        // SAFETY: signal zero checks group existence without changing it.
        if unsafe { libc::kill(-id, 0) } == 0 { return Ok(true); }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false), Some(libc::EPERM) => Ok(true), _ => Err(error),
        }
    }
    #[cfg(not(unix))] { let _ = id; Ok(false) }
}

async fn wait_for_group(child: &mut tokio::process::Child, id: u32, grace: std::time::Duration) -> std::io::Result<bool> {
    let deadline = std::time::Instant::now() + grace;
    loop {
        child.try_wait()?;
        if !group_alive(id)? { return Ok(true); }
        if std::time::Instant::now() >= deadline { return Ok(false); }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

pub async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
    envelope: Option<(&[u8], usize)>,
) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    if (&mut *reader).take(limit as u64 + 1).read_until(b'\n', &mut line).await? == 0 {
        return Ok(None);
    }
    let limit = envelope.filter(|(prefix, _)| line.starts_with(prefix))
        .map_or(limit, |(_, envelope)| envelope.max(limit));
    if line.len() <= limit && !line.ends_with(b"\n") {
        (&mut *reader).take((limit + 1 - line.len()) as u64).read_until(b'\n', &mut line).await?;
    }
    let cut = line.len() > limit && !line.ends_with(b"\n");
    if cut {
        let mut piece = Vec::new();
        loop {
            piece.clear();
            let count = (&mut *reader).take(16 * 1024).read_until(b'\n', &mut piece).await?;
            if count == 0 || piece.ends_with(b"\n") { break; }
        }
    }
    let text = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_owned();
    Ok(Some(if cut || text.len() > limit { OMITTED_LINE.to_owned() } else { text }))
}
