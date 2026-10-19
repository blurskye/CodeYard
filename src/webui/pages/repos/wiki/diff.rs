//! A line diff between two revisions of a page, as the compare view lists it.

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LineKind {
    Same,
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
}

/// Above this many line pairs the table is too large to build, so the diff lists the old text as
/// removed and the new text as added.
const TABLE_LIMIT: usize = 4_000_000;

/// The lines of `old` and `new` in order, from a longest-common-subsequence table.
pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let line = |kind, text: &str| DiffLine {
        kind,
        text: text.to_owned(),
    };
    if a.len().saturating_mul(b.len()) > TABLE_LIMIT {
        return a
            .iter()
            .map(|text| line(LineKind::Removed, text))
            .chain(b.iter().map(|text| line(LineKind::Added, text)))
            .collect();
    }
    // lcs[i][j] is the length of the common subsequence of a[i..] and b[j..].
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(a.len() + b.len());
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(line(LineKind::Same, a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(line(LineKind::Removed, a[i]));
            i += 1;
        } else {
            out.push(line(LineKind::Added, b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|text| line(LineKind::Removed, text)));
    out.extend(b[j..].iter().map(|text| line(LineKind::Added, text)));
    out
}

#[cfg(test)]
mod tests {
    use super::{line_diff, LineKind};

    #[test]
    fn diff_marks_removed_and_added_lines_around_common_ones() {
        let diff = line_diff("one\ntwo\nthree", "one\n2\nthree\nfour");
        let kinds: Vec<(LineKind, &str)> = diff.iter().map(|d| (d.kind, d.text.as_str())).collect();
        assert_eq!(
            kinds,
            vec![
                (LineKind::Same, "one"),
                (LineKind::Removed, "two"),
                (LineKind::Added, "2"),
                (LineKind::Same, "three"),
                (LineKind::Added, "four"),
            ]
        );
    }
}
