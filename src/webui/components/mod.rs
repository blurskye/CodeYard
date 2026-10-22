mod avatar;
pub mod charts;
pub mod code;
mod confirm;
mod copy;
pub mod deploy_keys;
mod forms;
pub mod languages;
mod markdown;
mod markdown_html;
mod markdown_preview;
mod markdown_toolbar;
#[cfg(target_arch = "wasm32")]
pub mod menu;
mod patch;
pub mod references;
mod shell;
pub mod task_lists;
mod theme;
mod time;
pub mod webhooks;

pub use avatar::Avatar;
pub use confirm::confirm_dialog;
pub use copy::CopyButton;
pub use forms::{
    submit_form, DraftCheckbox, DraftInput, DraftSelect, DraftTextarea, FieldError, SelectOption,
};
pub use markdown::{
    document_html, escape_html, markdown_tab_class, outline_of_html, parse_markdown, LinkBase,
    Markdown, OutlineEntry, RepoMarkdown,
};
pub use markdown_preview::body_preview;
pub use markdown_toolbar::MarkdownToolbar;
pub use patch::patch_badges;
pub(crate) use patch::{patch_gutter_digits, patch_rows, PatchRow};
pub use shell::{shell, shell_at, SiteSearch};
pub use theme::{ThemeForm, ThemeSaved};
pub use time::{exact_time, LiveTime};

/// The wording of every count badge: "1 node", "0 nodes", "2 branches".
pub fn count_label(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// A date for prose and lists, as GitHub shows it: "Oct 2, 2026". An RFC 3339 time or a plain
/// `YYYY-MM-DD` date is formatted; any other text is kept as it is rather than invented.
/// The request-independent format keeps the server and the browser the same.
pub fn compact_date(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|date| date.format("%b %-d, %Y").to_string())
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .map(|date| date.format("%b %-d, %Y").to_string())
        })
        .unwrap_or_else(|_| value.to_owned())
}

/// The current time. Browsers have no `SystemTime` on wasm, so the page asks its own clock there.
pub fn current_time() -> chrono::DateTime<chrono::Utc> {
    #[cfg(target_arch = "wasm32")]
    {
        chrono::DateTime::from_timestamp_millis(js_sys::Date::now() as i64)
            .unwrap_or_else(chrono::Utc::now)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        chrono::Utc::now()
    }
}

/// A time as an age, as GitHub shows it: "3 days ago". `None` when `value` is not an RFC 3339
/// time, so the caller shows the text as it is.
pub fn relative_date(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let then = chrono::DateTime::parse_from_rfc3339(value)
        .ok()?
        .with_timezone(&chrono::Utc);
    Some(relative_age(then, now))
}

/// The age of `then` at `now`: "just now", "20 seconds ago", "5 minutes ago", "3 months ago".
pub fn relative_age(
    then: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let seconds = (now - then).num_seconds().max(0);
    let (count, unit) = match seconds {
        0..=1 => return "just now".to_owned(),
        2..=59 => (seconds, "second"),
        60..=3_599 => (seconds / 60, "minute"),
        3_600..=86_399 => (seconds / 3_600, "hour"),
        86_400..=2_591_999 => (seconds / 86_400, "day"),
        2_592_000..=31_535_999 => (seconds / 2_592_000, "month"),
        _ => (seconds / 31_536_000, "year"),
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural} ago")
}

/// The age of a Unix time in seconds, as "5 minutes ago". An out-of-range value reads "Unknown time".
pub fn relative_time(value: i64) -> String {
    chrono::DateTime::from_timestamp(value, 0)
        .map(|then| relative_age(then, current_time()))
        .unwrap_or_else(|| "Unknown time".to_string())
}

/// The first seven characters of a commit ID, or all of it when it is shorter.
pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// A byte count in binary units: "512 B", "148.5 KiB", "3.2 GiB".
pub fn format_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_counts_use_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(152_067), "148.5 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn ages_count_seconds_in_the_first_minute() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-02T09:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let age = |seconds| relative_age(now - chrono::Duration::seconds(seconds), now);
        assert_eq!(age(1), "just now");
        assert_eq!(age(20), "20 seconds ago");
        assert_eq!(age(60), "1 minute ago");
        assert_eq!(age(7_200), "2 hours ago");
    }

    #[test]
    fn count_badges_share_one_wording() {
        assert_eq!(count_label(0, "node", "nodes"), "0 nodes");
        assert_eq!(count_label(1, "node", "nodes"), "1 node");
        assert_eq!(count_label(2, "branch", "branches"), "2 branches");
    }
}
