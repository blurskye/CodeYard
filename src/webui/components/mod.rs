mod avatar;
pub mod code;
mod copy;
mod forms;
mod markdown;
mod markdown_html;
mod markdown_toolbar;
pub mod references;
mod shell;
mod theme;

pub use avatar::Avatar;
pub use copy::CopyButton;
pub use forms::{
    draft_text, DraftCheckbox, DraftInput, DraftSelect, DraftTextarea, FieldError, SelectOption,
};
pub use markdown::{
    escape_html, markdown_tab_class, parse_markdown, LinkBase, Markdown, RepoMarkdown,
};
pub use markdown_toolbar::MarkdownToolbar;
pub use shell::shell;
pub use theme::{ThemeForm, ThemeSaved};

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

/// One line of a patch. `old` and `new` are the line numbers drawn in the gutters; file
/// headers, hunk headers and the no-newline marker have none.
#[derive(Debug, PartialEq)]
pub(crate) struct PatchRow<'a> {
    pub(crate) class: &'static str,
    pub(crate) old: Option<u32>,
    pub(crate) new: Option<u32>,
    pub(crate) text: &'a str,
    /// A git header line before a file's first hunk. Pages show the file's header row instead.
    pub(crate) header: bool,
}

/// The lines git writes before a file's first hunk. Binary notices are not among them.
const GIT_HEADER_PREFIXES: [&str; 13] = [
    "index ",
    "--- ",
    "+++ ",
    "old mode ",
    "new mode ",
    "new file mode ",
    "deleted file mode ",
    "similarity index ",
    "dissimilarity index ",
    "rename from ",
    "rename to ",
    "copy from ",
    "copy to ",
];

/// The first old and new line of a hunk header such as `@@ -12,3 +14,4 @@ context`.
fn hunk_starts(header: &str) -> Option<(u32, u32)> {
    let (old, rest) = header.strip_prefix("@@ -")?.split_once(" +")?;
    let new = rest.split_once(" @@")?.0;
    let start = |range: &str| range.split(',').next()?.parse().ok();
    Some((start(old)?, start(new)?))
}

/// Classifies every line once and counts line numbers from the hunk headers, so it stays
/// linear in the patch size.
pub(crate) fn patch_rows(patch: &str) -> Vec<PatchRow<'_>> {
    let mut in_hunk = false;
    // The next old and new line number; none before the first hunk or after a header
    // that does not parse.
    let mut next: Option<(u32, u32)> = None;
    patch
        .split_inclusive('\n')
        .map(|text| {
            let mut row = PatchRow {
                class: "patch-line",
                old: None,
                new: None,
                text,
                header: false,
            };
            if text.starts_with("diff --git ") {
                // A type change holds two patch sections; the second one's file headers are not lines.
                in_hunk = false;
                next = None;
                row.header = true;
            } else if !in_hunk
                && GIT_HEADER_PREFIXES
                    .iter()
                    .any(|prefix| text.starts_with(prefix))
            {
                row.header = true;
            } else if text.starts_with("@@") {
                in_hunk = true;
                next = hunk_starts(text);
                row.class = "patch-line patch-hunk";
            } else if in_hunk && text.starts_with('+') {
                row.class = "patch-line patch-addition";
                if let Some((_, new)) = next.as_mut() {
                    row.new = Some(*new);
                    *new = new.saturating_add(1);
                }
            } else if in_hunk && text.starts_with('-') {
                row.class = "patch-line patch-deletion";
                if let Some((old, _)) = next.as_mut() {
                    row.old = Some(*old);
                    *old = old.saturating_add(1);
                }
            } else if in_hunk && (text.starts_with(' ') || text == "\n") {
                if let Some((old, new)) = next.as_mut() {
                    row.old = Some(*old);
                    row.new = Some(*new);
                    *old = old.saturating_add(1);
                    *new = new.saturating_add(1);
                }
            }
            row
        })
        .collect()
}

/// Keep patch prefixes and newlines intact; Dioxus escapes each line as text. The line
/// numbers are attributes that CSS draws, so copying a diff copies only its text.
/// The digit count the gutters must fit, when it is more than the four they fit by default.
/// CSS widens them for a change far into a very long file.
pub(crate) fn patch_gutter_digits(rows: &[PatchRow<'_>]) -> Option<String> {
    let digits = rows
        .iter()
        .flat_map(|row| [row.old, row.new])
        .flatten()
        .max()
        .map_or(0, |number| number.to_string().len());
    (digits > 4).then(|| digits.to_string())
}

pub fn patch_lines(patch: &str) -> dioxus::prelude::Element {
    use dioxus::prelude::*;
    let rows = patch_rows(patch);
    let digits = patch_gutter_digits(&rows);
    let lines = rows.into_iter().filter(|row| !row.header).map(|row| {
        let (class, text) = (row.class, row.text);
        let old = row.old.map(|number| number.to_string());
        let new = row.new.map(|number| number.to_string());
        rsx! { span { class, "data-old": old, "data-new": new, "{text}" } }
    });
    rsx! { code { class: "patch-lines", "data-digits": digits, {lines} } }
}

/// The badges of a changed file's header: what its status and git header say beyond the line
/// counts. A plain content change has none.
pub fn patch_badges(status: &str, patch: &str) -> Vec<String> {
    let header_value = |prefix: &str| patch.lines().find_map(|line| line.strip_prefix(prefix));
    let mut badges = Vec::new();
    match status {
        "added" => badges.push("New file".to_owned()),
        "removed" => badges.push("Deleted".to_owned()),
        "renamed" => badges.push(match header_value("rename from ") {
            Some(from) => format!("Renamed from {from}"),
            None => "Renamed".to_owned(),
        }),
        "type_changed" => badges.push("Type changed".to_owned()),
        _ => {}
    }
    if header_value("old mode ").is_some() {
        badges.push("Mode change".to_owned());
    }
    badges
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
    fn count_badges_share_one_wording() {
        assert_eq!(count_label(0, "node", "nodes"), "0 nodes");
        assert_eq!(count_label(1, "node", "nodes"), "1 node");
        assert_eq!(count_label(2, "branch", "branches"), "2 branches");
    }

    fn numbers(patch: &str) -> Vec<(Option<u32>, Option<u32>)> {
        patch_rows(patch)
            .iter()
            .map(|row| (row.old, row.new))
            .collect()
    }

    #[test]
    fn context_added_and_removed_lines_are_numbered_from_the_hunk_header() {
        let patch =
            "--- a/f\n+++ b/f\n@@ -3,4 +10,4 @@ fn main\n keep\n-gone\n+new\n+newer\n tail\n";
        assert_eq!(
            numbers(patch),
            [
                (None, None),
                (None, None),
                (None, None),
                (Some(3), Some(10)),
                (Some(4), None),
                (None, Some(11)),
                (None, Some(12)),
                (Some(5), Some(13)),
            ]
        );
        let rows = patch_rows(patch);
        assert_eq!(rows[3].class, "patch-line");
        assert_eq!(rows[4].class, "patch-line patch-deletion");
        assert_eq!(rows[5].class, "patch-line patch-addition");
    }

    #[test]
    fn each_hunk_restarts_the_count_and_a_new_file_section_resets_it() {
        let patch = "@@ -1,2 +1,2 @@\n a\n-b\n+c\n@@ -40 +40 @@\n-x\n+y\ndiff --git a/g b/g\n--- a/g\n+++ b/g\n@@ -0,0 +1,2 @@\n+one\n+two\n";
        assert_eq!(
            numbers(patch),
            [
                (None, None),
                (Some(1), Some(1)),
                (Some(2), None),
                (None, Some(2)),
                (None, None),
                (Some(40), None),
                (None, Some(40)),
                (None, None),
                (None, None),
                (None, None),
                (None, None),
                (None, Some(1)),
                (None, Some(2)),
            ]
        );
    }

    #[test]
    fn the_no_newline_marker_and_an_unreadable_header_have_no_numbers() {
        let patch =
            "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file";
        assert_eq!(
            numbers(patch),
            [
                (None, None),
                (Some(1), None),
                (None, None),
                (None, Some(1)),
                (None, None),
            ]
        );
        // Combined-diff headers are not read, and then nothing in them is numbered.
        let combined = "@@@ -1,2 -1,2 +1,3 @@@\n  a\n+ b\n";
        assert!(numbers(combined).iter().all(|row| *row == (None, None)));
        // A blank context line may lose its leading space.
        assert_eq!(
            numbers("@@ -5,2 +5,2 @@\n\n-a\n")[1..],
            [(Some(5), Some(5)), (Some(6), None)]
        );
    }

    #[test]
    fn rendered_rows_keep_the_original_text_and_carry_numbers_as_attributes() {
        let html = dioxus_ssr::render_element(patch_lines("@@ -7 +7 @@\n a\n-b\n+<c>\n"));
        assert!(html.contains("<span class=\"patch-line patch-hunk\">@@ -7 +7 @@\n</span>"));
        assert!(
            html.contains("<span class=\"patch-line\" data-old=\"7\" data-new=\"7\"> a\n</span>")
        );
        assert!(
            html.contains("<span class=\"patch-line patch-deletion\" data-old=\"8\">-b\n</span>")
        );
        assert!(html.contains(
            "<span class=\"patch-line patch-addition\" data-new=\"8\">+&#60;c&#62;\n</span>"
        ));
    }

    #[test]
    fn wide_line_numbers_name_their_digit_count_and_ordinary_diffs_do_not() {
        let ordinary = dioxus_ssr::render_element(patch_lines("@@ -9999 +9999 @@\n a\n"));
        assert!(!ordinary.contains("data-digits"));
        let wide = dioxus_ssr::render_element(patch_lines("@@ -999999 +1000000 @@\n a\n"));
        assert!(wide.contains("<code class=\"patch-lines\" data-digits=\"7\">"));
    }

    #[test]
    fn git_headers_are_hidden_rows_and_binary_notices_stay_visible() {
        let patch = "diff --git a/old.txt b/new.txt\nsimilarity index 90%\nrename from old.txt\nrename to new.txt\nindex 1..2 100644\n--- a/old.txt\n+++ b/new.txt\n@@ -1 +1 @@\n--- kept\n+new\ndiff --git a/logo.png b/logo.png\nindex 3..4 100644\nBinary files a/logo.png and b/logo.png differ\n";
        let headers: Vec<bool> = patch_rows(patch).iter().map(|row| row.header).collect();
        assert_eq!(
            headers,
            [true, true, true, true, true, true, true, false, false, false, true, true, false]
        );
        let html = dioxus_ssr::render_element(patch_lines(patch));
        assert!(!html.contains("diff --git"));
        assert!(!html.contains("index 1..2"));
        assert!(html.contains("--- kept"));
        assert!(html.contains("Binary files a/logo.png and b/logo.png differ"));
    }

    #[test]
    fn file_badges_name_new_deleted_renamed_and_mode_changed_files() {
        assert_eq!(patch_badges("added", ""), ["New file"]);
        assert_eq!(patch_badges("removed", ""), ["Deleted"]);
        assert_eq!(
            patch_badges(
                "renamed",
                "diff --git a/a b/b\nrename from a\nrename to b\n"
            ),
            ["Renamed from a"]
        );
        assert_eq!(
            patch_badges("modified", "old mode 100644\nnew mode 100755\n"),
            ["Mode change"]
        );
        assert!(patch_badges("modified", "@@ -1 +1 @@\n-a\n+b\n").is_empty());
    }

    #[test]
    fn numbering_a_long_patch_is_linear() {
        let patch = format!("@@ -1,100000 +1,100000 @@\n{}", " x\n".repeat(100_000));
        let started = std::time::Instant::now();
        let rows = patch_rows(&patch);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(rows.last().unwrap().new, Some(100_000));
    }
}
