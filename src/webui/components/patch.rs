//! Reading a unified diff: line classes, line numbers and the badges of a file header.

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
    fn wide_line_numbers_name_their_digit_count_and_ordinary_diffs_do_not() {
        assert_eq!(
            patch_gutter_digits(&patch_rows("@@ -9999 +9999 @@\n a\n")),
            None
        );
        assert_eq!(
            patch_gutter_digits(&patch_rows("@@ -999999 +1000000 @@\n a\n")).as_deref(),
            Some("7")
        );
    }

    #[test]
    fn git_headers_are_hidden_rows_and_binary_notices_stay_visible() {
        let patch = "diff --git a/old.txt b/new.txt\nsimilarity index 90%\nrename from old.txt\nrename to new.txt\nindex 1..2 100644\n--- a/old.txt\n+++ b/new.txt\n@@ -1 +1 @@\n--- kept\n+new\ndiff --git a/logo.png b/logo.png\nindex 3..4 100644\nBinary files a/logo.png and b/logo.png differ\n";
        let headers: Vec<bool> = patch_rows(patch).iter().map(|row| row.header).collect();
        assert_eq!(
            headers,
            [true, true, true, true, true, true, true, false, false, false, true, true, false]
        );
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
