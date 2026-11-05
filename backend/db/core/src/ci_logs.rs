use anyhow::{ensure, Result};
use codeyard_model::ci::LogLine;
use codeyard_model::ci::MAX_RUN_LOG_BYTES;
use codeyard_model::ci::MAX_RUN_LOG_LINES;
pub const TRUNCATED_LINE: &str = "Log output truncated: this run reached its log limit.";

/// Bytes kept free for the truncation line, whose stage is at most 255 bytes.
pub const TRUNCATION_RESERVE: i64 = 255 + TRUNCATED_LINE.len() as i64;

/// Decides which lines of a batch a run stores, given the totals it has stored so far,
/// and returns them with the new totals. The batch that reaches a limit stores one
/// truncation line in place of the first line it cannot keep. The line total then moves
/// to the limit, which closes the log: later lines are dropped and no second marker is
/// written.
pub fn admit_log_lines(
    stored_lines: i64,
    stored_bytes: i64,
    batch: &[LogLine],
) -> (Vec<LogLine>, i64, i64) {
    let mut stored = Vec::new();
    let (mut lines, mut bytes) = (stored_lines, stored_bytes);
    if lines >= MAX_RUN_LOG_LINES {
        return (stored, lines, bytes);
    }
    for line in batch {
        let size = (line.stage.len() + line.line.len()) as i64;
        let full = lines + 1 >= MAX_RUN_LOG_LINES;
        let over_bytes = bytes + size + TRUNCATION_RESERVE > MAX_RUN_LOG_BYTES;
        if full || over_bytes {
            stored.push(LogLine {
                stage: line.stage.clone(),
                line: TRUNCATED_LINE.to_owned(),
                level: "warn".to_owned(),
                job_id: line.job_id,
                step_number: line.step_number,
            });
            return (stored, MAX_RUN_LOG_LINES, bytes);
        }
        stored.push(line.clone());
        lines += 1;
        bytes += size;
    }
    (stored, lines, bytes)
}
/// Replaces each control byte except tab, CR and LF with `?`. PostgreSQL cannot store NUL.
/// A replacement keeps the byte length, so the batch limits and the worker's byte count
/// match what is stored.
fn printable_log_text(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' | '\r' | '\n' => c,
            c if c.is_ascii_control() => '?',
            c => c,
        })
        .collect()
}

pub fn prepare_logs(lines: &[LogLine]) -> Result<Vec<LogLine>> {
    ensure!(
        !lines.is_empty() && lines.len() <= 64,
        "CI log batch exceeds ingestion limits"
    );
    // PostgreSQL text cannot store NUL. Count the printable bytes on both engines.
    let lines: Vec<_> = lines
        .iter()
        .map(|line| LogLine {
            stage: printable_log_text(&line.stage),
            line: printable_log_text(&line.line),
            level: line.level.clone(),
            job_id: line.job_id,
            step_number: line.step_number,
        })
        .collect();
    ensure!(
        lines.iter().all(|line| line.stage.len() <= 255
            && line.line.len() <= 16 * 1024
            && matches!(line.level.as_str(), "info" | "warn" | "error" | "debug")
            && line.job_id.is_none_or(|id| id > 0)
            && line
                .step_number
                .is_none_or(|number| line.job_id.is_some() && (1..=10000).contains(&number))),
        "CI log input exceeds ingestion limits"
    );
    ensure!(
        lines
            .iter()
            .map(|line| line.line.len() + line.stage.len())
            .sum::<usize>()
            <= 256 * 1024,
        "CI log batch exceeds ingestion limits"
    );
    Ok(lines)
}

/// Durable log bytes supplied by the application's local, S3 or storage-node backend.
/// Database transactions contain only their bounded metadata.
pub trait LogObjects: Send + Sync {
    fn put<'a>(
        &'a self,
        key: &'a str,
        digest: &'a str,
        bytes: Vec<u8>,
    ) -> futures_util::future::BoxFuture<'a, Result<()>>;
    fn get<'a>(
        &'a self,
        key: &'a str,
        length: usize,
    ) -> futures_util::future::BoxFuture<'a, Result<Vec<u8>>>;
}
