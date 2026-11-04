//! Runner stage names and the limit on what one run stores.

pub mod archive;

pub fn normalize_stage_name(stage: &str) -> String {
    stage.strip_prefix("act:").unwrap_or(stage).to_owned()
}

#[cfg(test)]
mod tests {

    use codeyard_db_core::ci_logs::admit_log_lines;
    use codeyard_db_core::ci_logs::TRUNCATED_LINE;
    use codeyard_db_core::ci_logs::TRUNCATION_RESERVE;
    use codeyard_model::ci::LogLine;
    use codeyard_model::ci::MAX_RUN_LOG_BYTES;
    use codeyard_model::ci::MAX_RUN_LOG_LINES;

    fn line(text: &str) -> LogLine {
        LogLine {
            stage: "ci.yml".into(),
            line: text.into(),
            level: "info".into(),
            ..Default::default()
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
