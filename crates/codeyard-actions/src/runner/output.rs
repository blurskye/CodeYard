//! Bounded output ownership. Job futures never perform blocking stdout writes, including
//! authenticated progress. Ordinary logs are best effort; losing essential progress fails closed.
use anyhow::{Context, Result};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot, watch};

enum Frame {
    Line(String),
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub(super) struct Output {
    sender: mpsc::Sender<Frame>,
    failed: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
    cancel: watch::Sender<bool>,
}

impl Output {
    pub fn start(cancel: watch::Sender<bool>) -> Result<(Self, tokio::task::JoinHandle<()>)> {
        let mut stdout = Stdout::open()?;
        let (sender, mut receiver) = mpsc::channel(512);
        let failed = Arc::new(AtomicBool::new(false));
        let queued = Arc::new(AtomicUsize::new(0));
        let output = Self { sender, failed: failed.clone(), queued: queued.clone(), cancel: cancel.clone() };
        let task = tokio::spawn(async move {
            while let Some(frame) = receiver.recv().await {
                match frame {
                    Frame::Line(mut line) => {
                        queued.fetch_sub(line.len(), Ordering::Relaxed);
                        line.push('\n');
                        if stdout.write(line.as_bytes()).await.is_err() {
                            failed.store(true, Ordering::Release);
                            let _ = cancel.send(true);
                            break;
                        }
                    }
                    Frame::Flush(done) => { let _ = done.send(()); }
                }
            }
        });
        Ok((output, task))
    }

    pub fn line(&self, line: String, essential: bool) {
        // Reserve room for transitions and completion authentication when logs are backed up.
        if !essential && self.sender.capacity() <= 64 { return; }
        let bytes = line.len();
        let reserved = bytes <= 1024 * 1024 && self.queued.try_update(Ordering::Relaxed, Ordering::Relaxed,
            |queued| queued.checked_add(bytes).filter(|total| *total <= 8 * 1024 * 1024)).is_ok();
        let sent = reserved && self.sender.try_send(Frame::Line(line)).is_ok();
        if reserved && !sent { self.queued.fetch_sub(bytes, Ordering::Relaxed); }
        if !sent && essential {
            self.failed.store(true, Ordering::Release);
            let _ = self.cancel.send(true);
        }
    }

    pub async fn flush(&self) -> Result<()> {
        let (done, received) = oneshot::channel();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            self.sender.send(Frame::Flush(done)).await.context("Runner output closed")?;
            received.await.context("Runner output closed")
        }).await.context("Runner output stalled")??;
        anyhow::ensure!(!self.failed.load(Ordering::Acquire), "Runner progress output failed");
        Ok(())
    }
}

#[cfg(unix)]
enum Stdout {
    Pipe(tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>),
    File(tokio::fs::File),
}

#[cfg(unix)]
impl Stdout {
    fn open() -> Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        // SAFETY: duplicate stdout with independent descriptor ownership.
        let raw = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        if raw < 0 { return Err(std::io::Error::last_os_error().into()); }
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut metadata) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if !matches!(metadata.st_mode & libc::S_IFMT, libc::S_IFIFO | libc::S_IFSOCK) {
            return Ok(Self::File(tokio::fs::File::from_std(std::fs::File::from(fd))));
        }
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self::Pipe(tokio::io::unix::AsyncFd::new(fd)?))
    }

    async fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        match self {
            Self::File(file) => { file.write_all(bytes).await?; file.flush().await }
            Self::Pipe(fd) => {
                while !bytes.is_empty() {
                    let mut ready = fd.writable().await?;
                    match ready.try_io(|fd| {
                        // SAFETY: bytes remains valid for this write; the descriptor is owned.
                        let count = unsafe { libc::write(fd.get_ref().as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
                        if count < 0 { Err(std::io::Error::last_os_error()) }
                        else { Ok(count as usize) }
                    }) {
                        Ok(Ok(0)) => return Err(std::io::ErrorKind::WriteZero.into()),
                        Ok(Ok(count)) => bytes = &bytes[count..],
                        Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Ok(Err(error)) => return Err(error),
                        Err(_) => continue,
                    }
                }
                Ok(())
            }
        }
    }
}

#[cfg(not(unix))]
struct Stdout(tokio::io::Stdout);
#[cfg(not(unix))]
impl Stdout {
    fn open() -> Result<Self> { Ok(Self(tokio::io::stdout())) }
    async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write_all(bytes).await?;
        self.0.flush().await
    }
}
