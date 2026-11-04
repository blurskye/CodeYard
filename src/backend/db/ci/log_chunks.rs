//! Compressed, immutable batches share the same line cursors on both database engines.
use crate::backend::db::models::CiLogLine;
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use codeyard_model::ci::LogLine;
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const MAX_ENCODED_BYTES: usize = 2 * 1024 * 1024;
// A truncation marker can be longer than the line it replaces in an otherwise full batch.
const MAX_STORED_BATCH_BYTES: i64 = 256 * 1024 + 512;

#[derive(sqlx::FromRow)]
pub(crate) struct Chunk {
    pub run_id: i64,
    pub job_id: Option<i64>,
    pub first_line: i64,
    pub line_count: i64,
    pub byte_size: i64,
    pub data: Vec<u8>,
    pub created_at: DateTime<Utc>,
}

pub(crate) fn batch_job(lines: &[LogLine]) -> Option<i64> {
    lines
        .first()
        .and_then(|line| line.job_id)
        .filter(|job| lines.iter().all(|line| line.job_id == Some(*job)))
}

pub(crate) fn request_hash(lines: &[LogLine]) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(lines)?)))
}

pub(crate) fn encode(lines: &[LogLine]) -> Result<(Vec<u8>, i64)> {
    let bytes = serde_json::to_vec(lines)?;
    ensure!(
        bytes.len() <= MAX_ENCODED_BYTES,
        "CI chunk exceeds encoding limit"
    );
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&bytes)?;
    let data = encoder.finish()?;
    ensure!(
        data.len() <= MAX_ENCODED_BYTES,
        "CI chunk exceeds storage limit"
    );
    let size = lines
        .iter()
        .map(|line| line.stage.len() + line.line.len())
        .sum::<usize>();
    ensure!(
        size as i64 <= MAX_STORED_BATCH_BYTES,
        "CI chunk exceeds byte limit"
    );
    Ok((data, size as i64))
}

pub(crate) struct LogRange {
    pub after: i64,
    pub before: i64,
    limit: usize,
    reverse: bool,
    lines: Vec<CiLogLine>,
}

impl LogRange {
    pub(crate) fn new(after: i64, before: i64, limit: i64, reverse: bool) -> Result<Self> {
        ensure!(
            after >= 0 && (0..=100_000).contains(&before) && (0..=100_000).contains(&limit),
            "invalid CI log range"
        );
        Ok(Self {
            after,
            before,
            limit: limit as usize,
            reverse,
            lines: Vec::new(),
        })
    }

    pub(crate) fn complete(&self) -> bool {
        self.after >= self.before || self.lines.len() == self.limit
    }

    pub(crate) fn extend(&mut self, chunks: Vec<Chunk>) -> Result<()> {
        for chunk in chunks {
            let first = chunk.first_line;
            let count = chunk.line_count;
            let mut decoded = chunk.decode()?;
            let last = first + count - 1;
            if self.reverse {
                decoded.reverse();
            }
            for line in decoded {
                if line.id > self.after && line.id <= self.before {
                    self.lines.push(line);
                    if self.lines.len() == self.limit {
                        return Ok(());
                    }
                }
            }
            if self.reverse {
                self.before = first - 1;
            } else {
                self.after = last;
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Vec<CiLogLine> {
        self.lines
    }
}

impl Chunk {
    pub(crate) fn decode(self) -> Result<Vec<CiLogLine>> {
        ensure!(
            (1..=100_000).contains(&self.first_line)
                && (1..=64).contains(&self.line_count)
                && self.first_line + self.line_count <= 100_001
                && (0..=MAX_STORED_BATCH_BYTES).contains(&self.byte_size)
                && self.data.len() <= MAX_ENCODED_BYTES,
            "invalid CI chunk metadata"
        );
        let mut bytes = Vec::new();
        ZlibDecoder::new(self.data.as_slice())
            .take(MAX_ENCODED_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_ENCODED_BYTES,
            "CI chunk exceeds decoding limit"
        );
        let lines: Vec<LogLine> = serde_json::from_slice(&bytes)?;
        ensure!(
            lines.len() as i64 == self.line_count,
            "CI chunk line count differs"
        );
        let mut prepared = Vec::with_capacity(lines.len());
        for line in lines {
            prepared.extend(crate::backend::ci::queue::prepare_logs(&[line])?);
        }
        let lines = prepared;
        ensure!(
            batch_job(&lines) == self.job_id
                && lines
                    .iter()
                    .map(|line| (line.stage.len() + line.line.len()) as i64)
                    .sum::<i64>()
                    == self.byte_size,
            "CI chunk contents differ from metadata"
        );
        Ok(lines
            .into_iter()
            .enumerate()
            .map(|(offset, line)| CiLogLine {
                id: self.first_line + offset as i64,
                run_id: self.run_id,
                job_id: line.job_id,
                step_number: line.step_number,
                stage: line.stage,
                line: line.line,
                level: line.level,
                created_at: self.created_at,
            })
            .collect())
    }
}
