//! One bounded length-prefixed JSON request, then directional binary frames.
//! No pack is accumulated: each frame is at most 32 KiB and IO is backpressured.
use super::validation::{RefUpdate, MAX_HEADER};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{timeout, Duration};

pub(crate) const CHUNK: usize = 32 * 1024;
pub(crate) const STDIN: u8 = 0;
pub(crate) const EOF: u8 = 1;
pub(crate) const STDOUT: u8 = 2;
pub(crate) const STDERR: u8 = 3;
pub(crate) const EXIT: u8 = 4;
pub(crate) const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const SESSION_TIMEOUT: Duration =
    crate::backend::git::repository::process::SESSION_TIMEOUT;
const IO_TIMEOUT: Duration = crate::backend::git::repository::process::IDLE_TIMEOUT;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Request {
    Keys {
        user: String,
        key_type: String,
        base64_key: String,
        serve_command: String,
    },
    Serve {
        key_id: i64,
        command: String,
        git_protocol: Option<String>,
    },
    Hook {
        key_id: i64,
        repo_id: i64,
        token: String,
        post: bool,
        updates: Vec<RefUpdate>,
        /// `GIT_QUARANTINE_PATH` of a pre-receive hook. The broker checks it before use.
        #[serde(default)]
        quarantine: Option<String>,
    },
}

pub(crate) async fn write_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    request: &Request,
) -> Result<()> {
    let data = serde_json::to_vec(request)?;
    ensure!(data.len() <= MAX_HEADER, "control request too large");
    timeout(Duration::from_secs(10), async {
        writer.write_u32(data.len() as u32).await?;
        writer.write_all(&data).await?;
        writer.flush().await
    })
    .await??;
    Ok(())
}

pub(crate) async fn read_request<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Request> {
    timeout(Duration::from_secs(10), async {
        let len = reader.read_u32().await? as usize;
        ensure!(
            len > 0 && len <= MAX_HEADER,
            "invalid control request length"
        );
        let mut data = vec![0; len];
        reader.read_exact(&mut data).await?;
        Ok(serde_json::from_slice(&data)?)
    })
    .await?
}

fn validate_frame(tag: u8, len: usize) -> Result<()> {
    ensure!(len <= CHUNK, "frame too large");
    ensure!(
        match tag {
            STDIN | STDOUT | STDERR => len > 0,
            EOF => len == 0,
            EXIT => len == 4,
            _ => false,
        },
        "invalid frame"
    );
    Ok(())
}

pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    tag: u8,
    bytes: &[u8],
) -> Result<()> {
    validate_frame(tag, bytes.len())?;
    timeout(IO_TIMEOUT, async {
        writer.write_u8(tag).await?;
        writer.write_u32(bytes.len() as u32).await?;
        writer.write_all(bytes).await?;
        writer.flush().await
    })
    .await??;
    Ok(())
}

pub(crate) async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<(u8, Vec<u8>)> {
    // Session traffic can be one-way for minutes. The process owns the shared
    // idle timer; control callers wrap the entire exchange in their own timeout.
    let tag = reader.read_u8().await?;
    let len = reader.read_u32().await? as usize;
    validate_frame(tag, len)?;
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).await?;
    Ok((tag, bytes))
}

pub(crate) async fn finish<W: AsyncWrite + Unpin>(writer: &mut W, status: i32) -> Result<()> {
    write_frame(writer, EXIT, &status.to_be_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn framing_keeps_binary_stdout_and_stderr_separate() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        write_frame(&mut writer, STDOUT, b"PACK\0\xff")
            .await
            .unwrap();
        write_frame(&mut writer, STDERR, b"warning\n")
            .await
            .unwrap();
        finish(&mut writer, 7).await.unwrap();
        assert_eq!(
            read_frame(&mut reader).await.unwrap(),
            (STDOUT, b"PACK\0\xff".to_vec())
        );
        assert_eq!(
            read_frame(&mut reader).await.unwrap(),
            (STDERR, b"warning\n".to_vec())
        );
        assert_eq!(
            read_frame(&mut reader).await.unwrap(),
            (EXIT, 7i32.to_be_bytes().to_vec())
        );
    }

    #[tokio::test]
    async fn oversized_headers_and_frames_are_rejected_before_payload_allocation() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_u32((MAX_HEADER + 1) as u32).await.unwrap();
        assert!(read_request(&mut reader).await.is_err());
        writer.write_u8(STDOUT).await.unwrap();
        writer.write_u32((CHUNK + 1) as u32).await.unwrap();
        assert!(read_frame(&mut reader).await.is_err());
        assert!(validate_frame(EXIT, 0).is_err());
        assert!(validate_frame(255, 1).is_err());
        assert!(validate_frame(EOF, 1).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"operation":"serve","key_id":1,"command":"x","git_protocol":null,"hooksPath":"evil"}"#).is_err());
    }
}
