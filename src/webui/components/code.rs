//! Native, pure subset of the approved code viewer. Only generated escaped spans enter HTML.
use std::{str::FromStr, sync::OnceLock};
use syntect::{
    easy::HighlightLines,
    highlighting::{
        Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
    },
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

pub fn escape_code(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The language a file is highlighted as, from the extension of its name. A name without an
/// extension gives "text", which is shown plain.
pub fn language_of_path(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map_or("text", |(_, extension)| extension)
}

fn extension(language: &str) -> &str {
    match language {
        "typescript" | "ts" | "mts" | "cts" => "ts",
        "javascript" | "js" | "mjs" | "cjs" => "js",
        "python" | "py" => "py",
        "yaml" | "yml" => "yaml",
        "bash" | "shellscript" | "sh" | "shell" => "sh",
        "rust" | "rs" => "rs",
        "markdown" | "md" => "md",
        "json" | "html" | "css" | "svelte" | "go" => language,
        _ => "",
    }
}

fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| Theme {
        scopes: [
            ("comment", "8b949e", true),
            ("string", "a5d6ff", false),
            ("constant", "79c0ff", false),
            ("keyword", "ff7b72", false),
            ("storage", "ff7b72", false),
            ("entity.name.function", "d2a8ff", false),
            ("entity.name.type", "ffa657", false),
            ("entity.name.tag", "7ee787", false),
            ("variable", "ffa657", false),
            ("punctuation", "c9d1d9", false),
        ]
        .into_iter()
        .map(|(scope, hex, italic)| ThemeItem {
            scope: ScopeSelectors::from_str(scope).unwrap_or_default(),
            style: StyleModifier {
                foreground: Some(Color {
                    r: u8::from_str_radix(&hex[..2], 16).unwrap(),
                    g: u8::from_str_radix(&hex[2..4], 16).unwrap(),
                    b: u8::from_str_radix(&hex[4..], 16).unwrap(),
                    a: 255,
                }),
                background: None,
                font_style: if italic {
                    Some(FontStyle::ITALIC)
                } else {
                    None
                },
            },
        })
        .collect(),
        // Text no scope above matches would otherwise come out black on the dark theme.
        settings: ThemeSettings {
            foreground: Some(Color {
                r: 0xc9,
                g: 0xd1,
                b: 0xd9,
                a: 255,
            }),
            ..ThemeSettings::default()
        },
        ..Theme::default()
    })
}

pub fn highlight(code: &str, language: &str) -> String {
    highlight_each_line(code, language).concat()
}

/// The highlighted HTML of each line of `code`, without its newline, for a view that gives
/// every line its own table cell.
pub fn highlight_lines(code: &str, language: &str) -> Vec<String> {
    highlight_each_line(code, language)
        .into_iter()
        .map(|line| line.replace('\n', ""))
        .collect()
}

/// The highlighted HTML of each line of `code`, newline included. Syntax state carries from
/// one line to the next, so a comment that spans lines is coloured on every line it covers.
fn highlight_each_line(code: &str, language: &str) -> Vec<String> {
    const MAX_BYTES: usize = 64 * 1024;
    const MAX_LINE_BYTES: usize = 64 * 1024;
    const MAX_TOKENS: usize = 4096;
    const MAX_HTML_BYTES: usize = 256 * 1024;
    let plain = || LinesWithEndings::from(code).map(escape_code).collect();
    if code.len() > MAX_BYTES || code.lines().any(|line| line.len() > MAX_LINE_BYTES) {
        return plain();
    }
    static SYNTAX: OnceLock<SyntaxSet> = OnceLock::new();
    let set = SYNTAX.get_or_init(SyntaxSet::load_defaults_newlines);
    let normalized = language.trim().to_lowercase();
    let syntax = set
        .find_syntax_by_extension(extension(&normalized))
        .unwrap_or_else(|| set.find_syntax_plain_text());
    if syntax.name == set.find_syntax_plain_text().name {
        return LinesWithEndings::from(code).map(escape_code).collect();
    }
    let mut highlighter = HighlightLines::new(syntax, theme());
    let mut lines = Vec::new();
    let mut token_count = 0;
    let mut output_bytes = 0;
    for line in LinesWithEndings::from(code) {
        let Ok(tokens) = highlighter.highlight_line(line, set) else {
            return plain();
        };
        token_count += tokens.len();
        if token_count > MAX_TOKENS {
            return plain();
        }
        let mut html = String::new();
        for (style, token) in tokens {
            let span = format!(
                "<span style=\"color:#{:02x}{:02x}{:02x}\">{}</span>",
                style.foreground.r,
                style.foreground.g,
                style.foreground.b,
                escape_code(token)
            );
            output_bytes += span.len();
            if output_bytes > MAX_HTML_BYTES {
                return plain();
            }
            html.push_str(&span);
        }
        lines.push(html);
    }
    lines
}

/// Map only generated token tags to theme variables; untrusted source stays escaped.
pub fn themed_highlight(content: &str, language: &str) -> String {
    themed(highlight(content, language))
}

/// [`themed_highlight`] for each line, as [`highlight_lines`] splits them.
pub fn themed_highlight_lines(content: &str, language: &str) -> Vec<String> {
    highlight_lines(content, language)
        .into_iter()
        .map(themed)
        .collect()
}

fn themed(mut html: String) -> String {
    for (hex, token) in [
        ("8b949e", "comment"),
        ("a5d6ff", "string"),
        ("79c0ff", "constant"),
        ("ff7b72", "keyword"),
        ("d2a8ff", "function"),
        ("ffa657", "variable"),
        ("7ee787", "tag"),
        ("c9d1d9", "text"),
    ] {
        html = html.replace(
            &format!("<span style=\"color:#{hex}\">"),
            &format!("<span style=\"color:var(--repo-code-{token})\">"),
        );
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn highlighted_lines_have_no_newline_and_keep_multi_line_comments() {
        let lines = highlight_lines("/* one\ntwo */\nlet x = 1;\n", "rs");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|line| !line.contains('\n')));
        assert!(lines[1].contains("two"));
        assert_eq!(highlight("a < b\n", "txt"), "a &lt; b\n");
        assert_eq!(highlight_lines("a < b\nc\n", "txt"), ["a &lt; b", "c"]);
    }

    #[test]
    fn expensive_highlighting_falls_back_to_escaped_lines() {
        for code in [
            format!("[{}]\n<end>\n", "0,".repeat(450_000)),
            format!("{}\n", "let x = 0;\n".repeat(10_000)),
            "[0,0,0,0,0,0,0,0]\n".repeat(500),
        ] {
            assert_eq!(highlight(&code, "json"), escape_code(&code));
            assert_eq!(highlight_lines(&code, "json").len(), code.lines().count());
        }
    }

    #[test]
    fn a_file_name_gives_its_highlighting_language() {
        assert_eq!(language_of_path("src/main.rs"), "rs");
        assert_eq!(language_of_path("docs/Guide.MD"), "MD");
        assert_eq!(language_of_path("dir.v2/README"), "text");
        assert_eq!(language_of_path("Makefile"), "text");
        assert_eq!(language_of_path(""), "text");
    }

    #[test]
    fn unknown_extensions_remain_escaped_as_in_original() {
        assert_eq!(
            highlight("<script>&</script>", "xyz"),
            "&lt;script&gt;&amp;&lt;/script&gt;"
        );
    }
    #[test]
    fn text_without_a_matching_scope_is_not_black() {
        let html = highlight("fn main() {\n    println!(\"hello\");\n}\n", "rs");
        assert!(!html.contains("color:#000000"));
        assert!(html.contains("color:#c9d1d9"));
    }
    #[test]
    fn file_extensions_of_listed_languages_are_highlighted() {
        for extension in ["rs", "md", "py", "js"] {
            let html = highlight("fn main() {}\n", extension);
            assert!(html.contains("<span style="), "{extension}");
        }
    }
    #[test]
    fn highlighting_escapes_html_and_unknown_languages_work() {
        let html = highlight("<script>alert(1)</script>", "nonexistent");
        assert!(!html.contains("<script"));
        assert!(html.contains("&lt;script&gt;"));
        assert_eq!(html, "&lt;script&gt;alert(1)&lt;/script&gt;");
    }

    #[test]
    fn tokens_do_not_allow_markup_and_keep_newlines() {
        let html = highlight("let x = '<script>';\n", "javascript");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert_eq!(html.matches('\n').count(), 1);
    }
}
