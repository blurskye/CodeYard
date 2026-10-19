//! Runner stage names and the limit on what one run stores.
use super::queue::LogLine;

/// Lines one run may store, counting its truncation line.
pub const MAX_RUN_LOG_LINES: i64 = 100_000;
/// Stage and line bytes one run may store.
pub const MAX_RUN_LOG_BYTES: i64 = 32 * 1024 * 1024;
const TRUNCATED_LINE: &str = "Log output truncated: this run reached its log limit.";
/// Bytes kept free for the truncation line, whose stage is at most 255 bytes.
const TRUNCATION_RESERVE: i64 = 255 + TRUNCATED_LINE.len() as i64;

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
            });
            return (stored, MAX_RUN_LOG_LINES, bytes);
        }
        stored.push(line.clone());
        lines += 1;
        bytes += size;
    }
    (stored, lines, bytes)
}

pub fn normalize_stage_name(stage: &str) -> String {
    stage.strip_prefix("act:").unwrap_or(stage).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> LogLine {
        LogLine {
            stage: "ci.yml".into(),
            line: text.into(),
            level: "info".into(),
        }
    }

    #[test]
    fn batches_under_the_limits_are_stored_whole() {
        let (stored, lines, bytes) = admit_log_lines(10, 100, &[line("one"), line("two")]);
        assert_eq!(stored.len(), 2);
        assert_eq!(lines, 12);
        assert_eq!(bytes, 100 + 2 * (6 + 3));
    }

    #[test]
    fn reaching_the_line_limit_stores_one_marker_and_closes_the_log() {
        let batch = [line("kept"), line("dropped"), line("dropped too")];
        let (stored, lines, _) = admit_log_lines(MAX_RUN_LOG_LINES - 2, 0, &batch);
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].line, "kept");
        assert_eq!(stored[1].line, TRUNCATED_LINE);
        assert_eq!(stored[1].level, "warn");
        assert_eq!(lines, MAX_RUN_LOG_LINES);

        let (later, closed, _) = admit_log_lines(lines, 0, &[line("late")]);
        assert!(later.is_empty());
        assert_eq!(closed, MAX_RUN_LOG_LINES);
    }

    #[test]
    fn reaching_the_byte_limit_stores_one_marker_and_closes_the_log() {
        let big = "x".repeat(16 * 1024);
        let used = MAX_RUN_LOG_BYTES - TRUNCATION_RESERVE - 100;
        let (stored, lines, _) = admit_log_lines(5, used, &[line(&big)]);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].line, TRUNCATED_LINE);
        assert_eq!(lines, MAX_RUN_LOG_LINES);
    }
}
