//! One bounded cat-file stream owns line counting and forward/backward preview boundaries.
use super::process;
use anyhow::{ensure, Context, Result};
use std::path::Path;
use tokio::io::AsyncReadExt;

pub(crate) const PREVIEW_BYTES: u64 = 256 * 1024;
pub(crate) const PREVIEW_LINES: usize = 2000;
pub(crate) struct PreviewRead {
    pub bytes: Vec<u8>,
    pub is_binary: bool,
    pub offset: u64,
    pub first_line: usize,
    pub previous: Option<u64>,
}

fn breaks(bytes: &[u8], end: usize) -> usize {
    bytes[..end].iter().enumerate().filter(|(index, byte)| {
        **byte == b'\n' || (**byte == b'\r' && bytes.get(*index + 1) != Some(&b'\n'))
    }).count()
}

/// Wait for enough input to apply both caps, retaining CRLF and complete UTF-8 tails.
fn window_end(bytes: &[u8], eof: bool) -> Option<usize> {
    let cap = PREVIEW_BYTES as usize;
    let mut end = bytes.len().min(cap);
    if bytes.get(cap - 1) == Some(&b'\r') && bytes.get(cap) == Some(&b'\n') {
        end = cap + 1;
    }
    let mut lines = 0;
    let mut line_limited = false;
    // The scan covers the byte window; a line limit only shortens `end` as it stops.
    let window = end;
    for index in 0..window {
        let byte = bytes[index];
        if byte == b'\r' && index + 1 == bytes.len() && !eof {
            return None;
        }
        if byte == b'\n' || (byte == b'\r' && bytes.get(index + 1) != Some(&b'\n')) {
            lines += 1;
            if lines == PREVIEW_LINES {
                end = index + 1;
                line_limited = true;
                break;
            }
        }
    }
    if !line_limited && bytes.len() <= cap && !eof {
        return None;
    }
    if !(eof && end == bytes.len()) {
        if let Some(lead) = (end.saturating_sub(3)..end).rev().find(|index| bytes[*index] & 0xC0 != 0x80) {
            let width = match bytes[lead] {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => 1,
            };
            if lead + width > end { end = lead; }
        }
    }
    Some(end)
}

struct Buffer {
    bytes: Vec<u8>,
    consumed: usize,
    read: u64,
    size: u64,
}
impl Buffer {
    fn remaining(&self) -> &[u8] { &self.bytes[self.consumed..] }
    async fn more(&mut self, stdout: &mut tokio::process::ChildStdout) -> Result<()> {
        if self.consumed > 0 {
            self.bytes.copy_within(self.consumed.., 0);
            self.bytes.truncate(self.bytes.len() - self.consumed);
            self.consumed = 0;
        }
        let old = self.bytes.len();
        let count = (self.size - self.read).min(process::CHUNK as u64)
            .min(PREVIEW_BYTES + 1 - old as u64) as usize;
        ensure!(count > 0, "Preview reader made no progress");
        self.bytes.resize(old + count, 0);
        let read = stdout.read(&mut self.bytes[old..]).await?;
        ensure!(read > 0, "Git blob ended before its declared size");
        self.bytes.truncate(old + read);
        self.read += read as u64;
        Ok(())
    }
    async fn window(&mut self, stdout: &mut tokio::process::ChildStdout) -> Result<usize> {
        loop {
            if let Some(end) = window_end(self.remaining(), self.read == self.size) { return Ok(end); }
            self.more(stdout).await?;
        }
    }
}

pub(crate) async fn read_blob_preview(
    path: &Path, id: &str, size: u64, offset: u64, target: Option<u32>,
) -> Result<PreviewRead> {
    ensure!(target != Some(0), "Invalid preview line");
    if size == 0 {
        return Ok(PreviewRead { bytes: Vec::new(), is_binary: false, offset: 0, first_line: 1, previous: None });
    }
    let oid = git2::Oid::from_str(id)?;
    let mut command = process::git_command()?;
    command.args(["cat-file", "blob", &oid.to_string()]).current_dir(path);
    let mut child = command.spawn().context("start git cat-file preview")?;
    let mut group = process::ProcessGroup::new(child.id(), std::time::Duration::ZERO);
    let mut stdout = child.stdout.take().context("git cat-file stdout")?;
    let stderr = child.stderr.take().context("git cat-file stderr")?;
    drop(child.stdin.take());
    let work = async {
        let mut buffer = Buffer { bytes: Vec::with_capacity(PREVIEW_BYTES as usize + 1), consumed: 0, read: 0, size };
        while buffer.read < size.min(8000) { buffer.more(&mut stdout).await?; }
        let binary = buffer.remaining()[..size.min(8000) as usize].contains(&0);
        let mut base = 0;
        let mut first_line = 1usize;
        let mut previous = None;
        let mut result = PreviewRead { bytes: Vec::new(), is_binary: binary, offset: 0, first_line: 1, previous: None };
        if !binary {
            loop {
                let end = buffer.window(&mut stdout).await?;
                let bytes = buffer.remaining();
                let skip = if let Some(target) = target {
                    if target as usize <= first_line { Some(0) } else {
                        let mut line = first_line;
                        (0..end).find_map(|index| {
                            if bytes[index] == b'\n' || (bytes[index] == b'\r' && bytes.get(index + 1) != Some(&b'\n')) {
                                line += 1;
                                if line == target as usize { return Some(index + 1); }
                            }
                            None
                        }).filter(|skip| *skip < end)
                    }
                } else if offset.min(size) < base + end as u64 { Some((offset.min(size) - base) as usize) }
                else { None };
                if let Some(mut skip) = skip {
                    // A hand-entered offset inside CRLF must not invent a blank source line.
                    if skip > 0 && bytes.get(skip - 1) == Some(&b'\r') && bytes.get(skip) == Some(&b'\n') { skip += 1; }
                    first_line += breaks(bytes, skip);
                    let start = base + skip as u64;
                    let back = if skip == 0 { previous } else { Some(base) };
                    buffer.consumed += skip;
                    let length = buffer.window(&mut stdout).await?;
                    result = PreviewRead { bytes: buffer.remaining()[..length].to_vec(), is_binary: false,
                        offset: start, first_line, previous: back };
                    break;
                }
                if end == 0 {
                    result.offset = size;
                    result.first_line = first_line;
                    result.previous = previous;
                    break;
                }
                first_line += breaks(bytes, end);
                previous = Some(base);
                base += end as u64;
                buffer.consumed += end;
            }
        }
        if buffer.read < size { process::kill_group(child.id()); }
        let status = child.wait().await?;
        let errors = process::read_bounded(stderr, 16 * 1024).await?;
        if buffer.read == size {
            ensure!(status.success(), "Git blob preview failed: {}", String::from_utf8_lossy(&errors));
        }
        Ok::<_, anyhow::Error>(result)
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), work).await.context("Git preview timed out")?;
    if result.is_ok() { group.disarm(); }
    result
}
