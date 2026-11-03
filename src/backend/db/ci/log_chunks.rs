//! Compressed, immutable batches share the same line cursors on both database engines.
use crate::backend::{ci::queue::LogLine, db::models::CiLogLine};
use anyhow::{ensure, Result};
use chrono::{DateTime, Utc};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const MAX_ENCODED_BYTES: usize = 2 * 1024 * 1024;

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
    Ok((data, size as i64))
}

impl Chunk {
    pub(crate) fn decode(self) -> Result<Vec<CiLogLine>> {
        ensure!(
            self.first_line > 0
                && (1..=64).contains(&self.line_count)
                && self.first_line + self.line_count <= 100_001
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
        let lines = crate::backend::ci::queue::prepare_logs(&lines)?;
        ensure!(
            lines.iter().all(|line| line.job_id == self.job_id)
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
