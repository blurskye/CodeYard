//! A Markdown body as one plain line, for list rows such as a notification or a search result.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const MAX_CHARS: usize = 160;
// A preview this short never needs more source than this, and parsing less keeps long bodies cheap.
const MAX_SOURCE_BYTES: usize = 4096;

/// The words of a Markdown body without its markup. List items and table cells are set apart with
/// " · ", and a long body is cut at a word with an ellipsis.
pub fn body_preview(body: &str) -> String {
    let mut end = body.len().min(MAX_SOURCE_BYTES);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut text = String::new();
    let mut set_apart = false;
    for event in Parser::new_ext(&body[..end], options) {
        match event {
            Event::Start(Tag::Item | Tag::TableCell) => set_apart = true,
            Event::Text(part) | Event::Code(part) | Event::Html(part) | Event::InlineHtml(part) => {
                if set_apart && !part.trim().is_empty() {
                    if !text.trim().is_empty() {
                        text.push_str(" · ");
                    }
                    set_apart = false;
                }
                text.push_str(&part);
            }
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::BlockQuote(_)
                | TagEnd::CodeBlock
                | TagEnd::Item
                | TagEnd::TableHead
                | TagEnd::TableRow,
            ) => text.push(' '),
            _ => {}
        }
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_CHARS {
        return collapsed;
    }
    let mut cut: String = collapsed.chars().take(MAX_CHARS - 1).collect();
    // Break at the last word boundary, unless the next character continues the word or the
    // first word alone is longer than the limit.
    if !collapsed[cut.len()..].starts_with(' ') {
        if let Some(space) = cut.rfind(' ') {
            cut.truncate(space);
        }
    }
    format!("{}…", cut.trim_end_matches(['·', ' ']))
}

#[cfg(test)]
mod tests {
    use super::body_preview;

    #[test]
    fn previews_show_words_without_markdown_markup() {
        assert_eq!(body_preview("**one nit**"), "one nit");
        assert_eq!(
            body_preview("Use `cargo test` and ~~not~~ _this_"),
            "Use cargo test and not this"
        );
        assert_eq!(
            body_preview("## Summary\n- first item\n> quoted line\n1. second"),
            "Summary · first item quoted line · second"
        );
        assert_eq!(
            body_preview("| Name | Status |\n|---|:--:|\n| build | ok |"),
            "Name · Status · build · ok"
        );
        assert_eq!(
            body_preview("See [the docs](https://example.test/x) and ![logo](/logo.png)"),
            "See the docs and logo"
        );
        assert_eq!(
            body_preview("@alice reviewed snake_case_names"),
            "@alice reviewed snake_case_names"
        );
        assert_eq!(body_preview("  \n\n  "), "");
    }

    #[test]
    fn long_previews_are_cut_on_a_character_boundary() {
        let short = body_preview(&"é".repeat(160));
        assert_eq!(short.chars().count(), 160);
        assert!(!short.ends_with('…'));
        let long = body_preview(&"word ".repeat(100));
        assert_eq!(long.chars().count(), 160);
        assert!(long.ends_with('…'));
        assert!(!long.contains("  "));
        assert!(!long[..long.len() - '…'.len_utf8()].ends_with(' '));
    }

    #[test]
    fn long_previews_break_at_a_word_boundary() {
        let cut = body_preview(&"abcdefghij ".repeat(20));
        assert_eq!(cut, format!("{}…", ["abcdefghij"; 14].join(" ")));
    }

    #[test]
    fn unmatched_brackets_stay_text_and_a_huge_body_is_cheap() {
        assert_eq!(body_preview("see [a [b (c"), "see [a [b (c");
        let body = "[".repeat(200_000);
        assert_eq!(body_preview(&body), format!("{}…", "[".repeat(159)));
        // The cut can fall inside a multi-byte character.
        assert_eq!(body_preview(&"é".repeat(5000)).chars().count(), 160);
    }

    #[test]
    fn previews_drop_fence_info_strings_and_short_table_dividers() {
        assert_eq!(
            body_preview("Run this:\n```rust\nlet x = 1;\n```\ndone"),
            "Run this: let x = 1; done"
        );
        assert_eq!(
            body_preview("| Name | Status |\n|-|:-|\n| build | ok |"),
            "Name · Status · build · ok"
        );
    }
}
