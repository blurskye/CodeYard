//! A bounded producer keeps ownership even when a downstream reader stops polling.
use futures_util::{Stream, StreamExt};
use std::{io, time::Duration};

#[derive(Clone, Copy)]
pub struct Limits {
    stall_seconds: u64,
    minimum_rate: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            stall_seconds: 30,
            minimum_rate: 16 * 1024,
        }
    }
}
impl Limits {
    pub fn new(stall_seconds: u64, minimum_rate: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=300).contains(&stall_seconds) && (1..=1024 * 1024 * 1024).contains(&minimum_rate),
            "Invalid download limits"
        );
        Ok(Self {
            stall_seconds,
            minimum_rate,
        })
    }
}

/// The producer owns the allocation or file permit. Its send deadline runs even
/// when Hyper stops polling the response. Only one small chunk remains queued. A reader
/// that falls below the minimum rate loses the download, so a trickle cannot hold a slot.
pub fn download_body(
    stream: impl Stream<Item = io::Result<bytes::Bytes>> + Send + 'static,
) -> axum::body::Body {
    download_body_with(Limits::default(), stream)
}

pub fn download_body_with(
    limits: Limits,
    stream: impl Stream<Item = io::Result<bytes::Bytes>> + Send + 'static,
) -> axum::body::Body {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        let mut stream = Box::pin(stream);
        let started = tokio::time::Instant::now();
        let mut sent = 0_u64;
        loop {
            // A stalled source loses the download too, so it cannot hold the permit.
            let chunk = tokio::select! {
                _ = sender.closed() => return Ok(()),
                chunk = tokio::time::timeout(Duration::from_secs(limits.stall_seconds), stream.next()) => match chunk {
                    Ok(Some(chunk)) => chunk?,
                    Ok(None) => return Ok(()),
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Download source stalled",
                        ))
                    }
                },
            };
            let allowed = Duration::from_secs(limits.stall_seconds + sent / limits.minimum_rate);
            let wait = allowed
                .saturating_sub(started.elapsed())
                .min(Duration::from_secs(limits.stall_seconds));
            sent += chunk.len() as u64;
            match tokio::time::timeout(wait, sender.send(chunk)).await {
                Ok(Ok(())) => (),
                Ok(Err(_)) => return Ok(()),
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Download timed out",
                    ))
                }
            }
        }
    }));
    axum::body::Body::from_stream(futures_util::stream::try_unfold(
        (receiver, task),
        |(mut receiver, mut task)| async move {
            match receiver.recv().await {
                Some(chunk) => Ok(Some((chunk, (receiver, task)))),
                None => {
                    (&mut task)
                        .await
                        .map_err(|_| io::Error::other("Download task unavailable"))??;
                    Ok::<_, io::Error>(None)
                }
            }
        },
    ))
}
