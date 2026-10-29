//! Shared bounded subprocess output framing. Oversized lines are omitted, never truncated:
//! retaining a prefix could expose part of a credential that whole-value masking cannot match.
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

pub const OMITTED_LINE: &str = "Warning: An oversized output line was omitted.";

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
