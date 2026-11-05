use anyhow::ensure;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::models::CiLogLine;
use codeyard_model::ci::LogLine;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha2::Digest;
use sha2::Sha256;
use std::io::Read;
use std::io::Write;

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
    pub object_key: String,
    pub object_bytes: i64,
    pub object_digest: String,
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

pub fn encode(lines: &[LogLine]) -> Result<(Vec<u8>, i64)> {
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
    pub fn new(after: i64, before: i64, limit: i64, reverse: bool) -> Result<Self> {
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

    pub async fn extend(&mut self, pool: &codeyard_db_core::DbPool, chunks: Vec<Chunk>) -> Result<()> {
        for chunk in chunks {
            let first = chunk.first_line;
            let count = chunk.line_count;
            let store = pool.log_objects()?;
            ensure!((1..=MAX_ENCODED_BYTES as i64).contains(&chunk.object_bytes), "invalid CI object length");
            let data = store.get(&chunk.object_key, chunk.object_bytes as usize).await?;
            ensure!(data.len() == chunk.object_bytes as usize
                && hex::encode(Sha256::digest(&data)) == chunk.object_digest,
                "CI object differs from its metadata");
            let mut decoded = chunk.decode(data)?;
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

    pub fn finish(self) -> Vec<CiLogLine> {
        self.lines
    }
}

impl Chunk {
    pub fn decode(self, data: Vec<u8>) -> Result<Vec<CiLogLine>> {
        ensure!(
            (1..=100_000).contains(&self.first_line)
                && (1..=64).contains(&self.line_count)
                && self.first_line + self.line_count <= 100_001
                && (0..=MAX_STORED_BATCH_BYTES).contains(&self.byte_size)
                && data.len() <= MAX_ENCODED_BYTES,
            "invalid CI chunk metadata"
        );
        let mut bytes = Vec::new();
        ZlibDecoder::new(data.as_slice())
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
            prepared.extend(codeyard_db_core::ci_logs::prepare_logs(&[line])?);
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

pub(crate) enum Preparation {
    Complete(bool),
    Upload { lines: i64, bytes: i64 },
}
pub(crate) struct StoredChunk {
    pub key: String,
    pub length: i64,
    pub digest: String,
    pub stored_lines: i64,
    pub stored_bytes: i64,
}

pub(crate) async fn append(
    pool: &codeyard_db_core::DbPool, run: i64, worker: &str, sequence: i64, lines: &[LogLine],
) -> Result<bool> {
    use codeyard_db_core::DbPool;
    ensure!((0..100_000).contains(&sequence), "invalid CI upload sequence");
    let lines = codeyard_db_core::ci_logs::prepare_logs(lines)?;
    let hash = request_hash(&lines)?;
    // Contention can only come from another upload to this same run. Leave reserved
    // objects for the ordinary expiry sweep if authority or allocation changes during I/O.
    for _ in 0..3 {
        let key = format!("ci-logs/{}", uuid::Uuid::new_v4());
        let prepared = match pool {
            DbPool::Postgres(native) => crate::postgres::ci::prepare_ci_log_upload(native, run, worker, sequence, &hash, &key).await?,
            DbPool::Sqlite(native) => crate::sqlite::ci::prepare_ci_log_upload(native, run, worker, sequence, &hash, &key).await?,
        };
        let (stored_lines, stored_bytes) = match prepared {
            Preparation::Complete(open) => return Ok(open),
            Preparation::Upload { lines, bytes } => (lines, bytes),
        };
        let (kept, _, _) = codeyard_db_core::ci_logs::admit_log_lines(stored_lines, stored_bytes, &lines);
        let (data, _) = encode(&kept)?;
        let upload = StoredChunk {
            key, length: data.len() as i64, digest: hex::encode(Sha256::digest(&data)), stored_lines, stored_bytes,
        };
        pool.log_objects()?.put(&upload.key, &upload.digest, data).await?;
        // Recheck the worker, jobs and sequence in the committing transaction.
        let committed = match pool {
            DbPool::Postgres(native) => crate::postgres::ci::commit_ci_logs(native, run, worker, sequence, &lines, &upload).await?,
            DbPool::Sqlite(native) => crate::sqlite::ci::commit_ci_logs(native, run, worker, sequence, &lines, &upload).await?,
        };
        if let Some(open) = committed { return Ok(open); }
    }
    anyhow::bail!("CI log allocation changed; retry the batch")
}

pub(crate) async fn read_range(
    pool: &codeyard_db_core::DbPool, run: i64, after: i64, before: i64, limit: i64, reverse: bool,
) -> Result<Vec<CiLogLine>> {
    use codeyard_db_core::DbPool;
    let mut range = LogRange::new(after, before, limit, reverse)?;
    while !range.complete() {
        // Queries return metadata only and release their connection before object I/O.
        let chunks = match pool {
            DbPool::Postgres(native) => crate::postgres::ci::read_log_chunks(native, run, range.after, range.before, reverse).await?,
            DbPool::Sqlite(native) => crate::sqlite::ci::read_log_chunks(native, run, range.after, range.before, reverse).await?,
        };
        if chunks.is_empty() { break; }
        range.extend(pool, chunks).await?;
    }
    Ok(range.finish())
}
