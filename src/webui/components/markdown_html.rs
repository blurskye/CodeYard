//! Raw HTML in Markdown: a tokenizer for the HTML of one block or inline event, the allow-list
//! of elements and attributes, and the checks on attribute values. The renderer writes only what
//! these return, so source bytes reach the page as escaped text or as validated values.

use super::markdown::{fragment_slug, link_href, safe_url};
use std::collections::HashSet;

/// One part of the HTML in a block or an inline event.
#[derive(Debug, PartialEq)]
pub(super) enum Piece<'a> {
    /// Text, with its character references still to be decoded.
    Text(&'a str),
    /// Source that is shown as written: a `<` that does not begin a complete allowed tag, up to
    /// the next `<`.
    Literal(&'a str),
    /// A complete start tag of an allowed element, with its raw attributes.
    Open {
        name: &'static str,
        attrs: Vec<(String, &'a str)>,
        raw: &'a str,
    },
    /// A complete end tag of an allowed element.
    Close { name: &'static str, raw: &'a str },
    /// A complete comment, which is dropped.
    Comment,
}

/// The pieces of `html`, read once from the start.
pub(super) fn pieces(html: &str) -> Pieces<'_> {
    Pieces { html, at: 0 }
}

pub(super) struct Pieces<'a> {
    html: &'a str,
    at: usize,
}

impl<'a> Iterator for Pieces<'a> {
    type Item = Piece<'a>;

    fn next(&mut self) -> Option<Piece<'a>> {
        let html = self.html;
        let start = self.at;
        if start >= html.len() {
            return None;
        }
        let (piece, end) = if html.as_bytes()[start] == b'<' {
            markup(html, start)
        } else {
            let end = next_lt(html, start);
            (Piece::Text(&html[start..end]), end)
        };
        self.at = end;
        Some(piece)
    }
}

/// The index of the next `<` at or after `from`, or the end of `html`.
fn next_lt(html: &str, from: usize) -> usize {
    html[from..].find('<').map_or(html.len(), |at| from + at)
}

/// The markup at `start`, which is a `<`, and where it ends. A tag that is not complete is
/// literal up to the next `<`, and a comment that never ends is literal to the end of the
/// block, so no byte is read again by a later tag.
fn markup(html: &str, start: usize) -> (Piece<'_>, usize) {
    let rest = &html[start..];
    if let Some(after) = rest.strip_prefix("<!--") {
        return match after.find("-->") {
            Some(at) => (Piece::Comment, start + 4 + at + 3),
            None => (Piece::Literal(rest), html.len()),
        };
    }
    let tag = if rest.starts_with("</") {
        close_tag(html, start)
    } else {
        open_tag(html, start)
    };
    tag.unwrap_or_else(|| {
        let end = next_lt(html, start + 1);
        (Piece::Literal(&html[start..end]), end)
    })
}

fn close_tag(html: &str, start: usize) -> Option<(Piece<'_>, usize)> {
    let bytes = html.as_bytes();
    let name_end = tag_name_end(bytes, start + 2)?;
    let name = element(&html[start + 2..name_end].to_ascii_lowercase())?;
    let end = skip_space(bytes, name_end);
    if bytes.get(end) != Some(&b'>') {
        return None;
    }
    let end = end + 1;
    Some((
        Piece::Close {
            name,
            raw: &html[start..end],
        },
        end,
    ))
}

fn open_tag(html: &str, start: usize) -> Option<(Piece<'_>, usize)> {
    let bytes = html.as_bytes();
    let name_end = tag_name_end(bytes, start + 1)?;
    let name = element(&html[start + 1..name_end].to_ascii_lowercase())?;
    let mut at = name_end;
    let mut attrs = Vec::new();
    loop {
        let spaced = skip_space(bytes, at);
        match bytes.get(spaced) {
            Some(b'>') => {
                at = spaced + 1;
                break;
            }
            // `/>` is read as the end of the tag. The slash has no effect on an element that
            // is not void, as in HTML.
            Some(b'/') if bytes.get(spaced + 1) == Some(&b'>') => {
                at = spaced + 2;
                break;
            }
            // An attribute needs white space before it, as in `<a href=x>`.
            Some(&first)
                if spaced > at && (first.is_ascii_alphabetic() || matches!(first, b'_' | b':')) =>
            {
                let (attr, end) = attribute(html, spaced)?;
                attrs.push(attr);
                at = end;
            }
            _ => return None,
        }
    }
    Some((
        Piece::Open {
            name,
            attrs,
            raw: &html[start..at],
        },
        at,
    ))
}

/// The attribute whose name starts at `from`: its lowercased name and raw value (empty when it
/// has none), and where it ends. A quoted value may not contain `<`, which keeps every tag
/// inside the text up to the next `<`.
fn attribute(html: &str, from: usize) -> Option<((String, &str), usize)> {
    let bytes = html.as_bytes();
    let name_end = from
        + bytes[from..]
            .iter()
            .take_while(|&&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'.' | b'-'))
            .count();
    let name = html[from..name_end].to_ascii_lowercase();
    let equals = skip_space(bytes, name_end);
    if bytes.get(equals) != Some(&b'=') {
        return Some(((name, ""), name_end));
    }
    let value = skip_space(bytes, equals + 1);
    match bytes.get(value) {
        Some(&quote @ (b'"' | b'\'')) => {
            let close = value
                + 1
                + bytes[value + 1..]
                    .iter()
                    .position(|&b| b == quote || b == b'<')?;
            if bytes[close] != quote {
                return None;
            }
            Some(((name, &html[value + 1..close]), close + 1))
        }
        _ => {
            let len = bytes[value..]
                .iter()
                .take_while(|&&b| !is_space(b) && b >= 0x20 && !b"\"'=<>`".contains(&b))
                .count();
            if len == 0 {
                return None;
            }
            Some(((name, &html[value..value + len]), value + len))
        }
    }
}

/// The end of the tag name that starts at `from`: a letter, then letters, digits and hyphens.
fn tag_name_end(bytes: &[u8], from: usize) -> Option<usize> {
    if !bytes.get(from)?.is_ascii_alphabetic() {
        return None;
    }
    Some(
        from + bytes[from..]
            .iter()
            .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'-')
            .count(),
    )
}

fn skip_space(bytes: &[u8], from: usize) -> usize {
    from + bytes[from..].iter().take_while(|&&b| is_space(b)).count()
}

/// The white space of HTML and CommonMark.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The allowed element called `name`, which is lowercase.
pub(super) fn element(name: &str) -> Option<&'static str> {
    ELEMENTS.iter().copied().find(|known| *known == name)
}

const ELEMENTS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "blockquote",
    "br",
    "caption",
    "cite",
    "code",
    "dd",
    "del",
    "details",
    "div",
    "dl",
    "dt",
    "em",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "picture",
    "pre",
    "q",
    "s",
    "samp",
    "small",
    "source",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
    "var",
];

/// Elements with no end tag.
pub(super) fn is_void(name: &str) -> bool {
    matches!(name, "br" | "hr" | "img" | "source")
}

/// Whether `name` may open inside `parent`, the innermost open element. Parts of a table or a
/// list are kept inside their container, where a browser would place them.
pub(super) fn may_nest(name: &str, parent: Option<&str>) -> bool {
    match name {
        "li" => matches!(parent, Some("ul" | "ol")),
        "dt" | "dd" => parent == Some("dl"),
        "tr" => matches!(parent, Some("table" | "thead" | "tbody" | "tfoot")),
        "th" | "td" => parent == Some("tr"),
        "thead" | "tbody" | "tfoot" | "caption" => parent == Some("table"),
        "summary" => parent == Some("details"),
        "source" => parent == Some("picture"),
        _ => true,
    }
}

/// The allowed attributes of `element` that have valid values, decoded, in source order with
/// a link or image address first. The first occurrence of a name wins. An `id` is recorded in
/// `ids`, so no heading or other element takes it.
pub(super) fn attributes(
    element: &str,
    attrs: &[(String, &str)],
    ids: &mut HashSet<String>,
) -> Vec<(&'static str, String)> {
    let mut seen: Vec<&'static str> = Vec::new();
    let mut out = Vec::new();
    for (name, raw) in attrs {
        let Some(name) = attribute_name(element, name) else {
            continue;
        };
        if seen.contains(&name) {
            continue;
        }
        seen.push(name);
        if let Some(value) = attribute_value(element, name, raw, ids) {
            out.push((name, value));
        }
    }
    out.sort_by_key(|(name, _)| !matches!(*name, "href" | "src" | "srcset"));
    out
}

/// The name an allowed attribute is kept under, or `None` when `element` may not set it.
fn attribute_name(element: &str, name: &str) -> Option<&'static str> {
    let aligned = matches!(
        element,
        "p" | "div"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "th"
            | "td"
            | "caption"
            | "img"
    );
    let cell = matches!(element, "td" | "th");
    Some(match name {
        "title" => "title",
        "lang" => "lang",
        "dir" => "dir",
        "id" => "id",
        "name" if element == "a" => "id",
        "href" if element == "a" => "href",
        "align" if aligned => "align",
        "src" if element == "img" => "src",
        "alt" if element == "img" => "alt",
        "width" if element == "img" => "width",
        "height" if element == "img" => "height",
        "colspan" if cell => "colspan",
        "rowspan" if cell => "rowspan",
        "start" if element == "ol" => "start",
        "open" if element == "details" => "open",
        "srcset" if element == "source" => "srcset",
        "media" if element == "source" => "media",
        "type" if element == "source" => "type",
        _ => return None,
    })
}

/// The value an allowed attribute takes, decoded and checked, or `None` when it is not valid.
fn attribute_value(
    element: &str,
    name: &str,
    raw: &str,
    ids: &mut HashSet<String>,
) -> Option<String> {
    let (text, unknown) = decode_references(raw);
    match name {
        // A URL with a reference this renderer does not know is dropped, not guessed at.
        "href" => link_href(&text).filter(|_| !unknown),
        "src" => (!unknown && safe_url(&text, true)).then(|| text.replace(' ', "%20")),
        "srcset" => {
            if unknown {
                None
            } else {
                source_url(&text)
            }
        }
        "id" => take_id(&text, ids),
        "title" | "alt" => Some(text),
        "lang" => (!text.is_empty()
            && text.len() <= 16
            && text.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-'))
        .then_some(text),
        "dir" => matches!(text.as_str(), "ltr" | "rtl" | "auto").then_some(text),
        "align" => {
            let values: &[&str] = if element == "img" {
                &["left", "right", "center", "top", "middle", "bottom"]
            } else {
                &["left", "center", "right", "justify"]
            };
            values.contains(&text.as_str()).then_some(text)
        }
        "width" | "height" => length(&text).then_some(text),
        "colspan" | "rowspan" => span(&text).then_some(text),
        "start" => ordinal(&text).then_some(text),
        // A boolean attribute: its presence is its value.
        "open" => Some(String::new()),
        "media" => {
            let lower = text.to_ascii_lowercase();
            media_query(&lower).then_some(lower)
        }
        "type" => {
            let lower = text.to_ascii_lowercase();
            image_type(&lower).then_some(lower)
        }
        _ => None,
    }
}

// A `srcset` with more candidates is dropped whole, since each candidate expands to a long URL.
const MAX_SRCSET_CANDIDATES: usize = 16;

/// The `srcset` of a source, as `address` or `address descriptor` candidates joined by `, `.
/// Every candidate needs a safe address and at most one valid descriptor, so a source with one
/// bad candidate is dropped and the image fallback shows.
fn source_url(text: &str) -> Option<String> {
    Some(
        srcset_candidates(text)?
            .into_iter()
            .map(|(url, descriptor)| {
                if descriptor.is_empty() {
                    url.to_owned()
                } else {
                    format!("{url} {descriptor}")
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// HTML consumes URLs up to whitespace, not commas within the URL token.
pub(super) fn srcset_candidates(mut text: &str) -> Option<Vec<(&str, &str)>> {
    let mut candidates = Vec::new();
    while !text.trim_ascii_start().is_empty() {
        text = text.trim_ascii_start();
        if candidates.len() >= MAX_SRCSET_CANDIDATES || text.starts_with(',') {
            return None;
        }
        let end = text
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(text.len());
        let token = &text[..end];
        text = &text[end..];
        let (url, desc) = if let Some(url) = token.strip_suffix(',') {
            // Keep the existing strict rejection of a trailing empty candidate.
            if text.trim_ascii_start().is_empty() {
                return None;
            }
            (url, "")
        } else {
            text = text.trim_ascii_start();
            let end = text.find(',').unwrap_or(text.len());
            let desc = text[..end].trim_ascii();
            text = text.get(end + 1..).unwrap_or("");
            (token, desc)
        };
        if !safe_url(url, true) || (!desc.is_empty() && !descriptor(desc)) {
            return None;
        }
        candidates.push((url, desc));
    }
    (!candidates.is_empty()).then_some(candidates)
}

/// A `srcset` descriptor: a width such as `100w`, or a density such as `2x` or `1.5x`.
fn descriptor(word: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if let Some(width) = word.strip_suffix('w') {
        return digits(width);
    }
    word.strip_suffix('x')
        .is_some_and(|density| match density.split_once('.') {
            Some((whole, fraction)) => digits(whole) && digits(fraction),
            None => digits(density),
        })
}

/// The `id` of an element: its slug under the `user-content-` prefix. An `id` that already has
/// the prefix keeps one copy of it, so a link to `#user-content-top` still reaches it. An empty
/// slug, or one an earlier element or heading already has, is dropped.
fn take_id(text: &str, ids: &mut HashSet<String>) -> Option<String> {
    let slug = fragment_slug(text.strip_prefix("user-content-").unwrap_or(text));
    if slug.is_empty() || !ids.insert(slug.clone()) {
        return None;
    }
    Some(format!("user-content-{slug}"))
}

/// A pixel count or a percentage of up to four digits.
fn length(text: &str) -> bool {
    let digits = text.strip_suffix('%').unwrap_or(text);
    (1..=4).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// A table cell span: one to three digits, no leading zero.
fn span(text: &str) -> bool {
    (1..=3).contains(&text.len())
        && !text.starts_with('0')
        && text.bytes().all(|b| b.is_ascii_digit())
}

/// A list start number, which may be negative.
fn ordinal(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    (1..=9).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// A media query of letters, digits and the marks queries use.
fn media_query(lower: &str) -> bool {
    (1..=80).contains(&lower.len())
        && lower
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b" ():,.-".contains(&b))
}

/// An image type such as `image/webp`.
fn image_type(lower: &str) -> bool {
    lower.strip_prefix("image/").is_some_and(|subtype| {
        !subtype.is_empty()
            && subtype
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+-.".contains(&b))
    })
}

/// `text` with its character references decoded, and whether it had one this renderer does not
/// know. An unknown reference is kept as written, so a caller that cannot use it can drop the
/// value. A `&` that does not begin a complete reference stands for itself.
pub(super) fn decode_references(text: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut unknown = false;
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match reference(&rest[1..]) {
            Some((Some(character), used)) => {
                out.push(character);
                rest = &rest[1 + used..];
            }
            Some((None, used)) => {
                unknown = true;
                out.push_str(&rest[..1 + used]);
                rest = &rest[1 + used..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    (out, unknown)
}

/// The reference that starts `after`, which follows its `&`: the character it stands for
/// (`None` when unknown) and its length, `;` included. `None` when no complete reference starts
/// there.
fn reference(after: &str) -> Option<(Option<char>, usize)> {
    if let Some(number) = after.strip_prefix('#') {
        let (radix, digits) = match number.strip_prefix(['x', 'X']) {
            Some(hex) => (16, hex),
            None => (10, number),
        };
        let len = digits
            .bytes()
            .take_while(|&b| (b as char).is_digit(radix))
            .count();
        if len == 0 || digits.as_bytes().get(len) != Some(&b';') {
            return None;
        }
        let character = u32::from_str_radix(&digits[..len], radix)
            .ok()
            .and_then(char::from_u32)
            .filter(|character| *character != '\0');
        return Some((character, after.len() - digits.len() + len + 1));
    }
    let len = after.bytes().take_while(u8::is_ascii_alphanumeric).count();
    if len == 0 || after.as_bytes().get(len) != Some(&b';') {
        return None;
    }
    Some((named(&after[..len]), len + 1))
}

/// The character of each named reference this renderer decodes. Any other name is unknown.
fn named(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "colon" => ':',
        "Tab" => '\t',
        "NewLine" => '\n',
        "nbsp" => '\u{a0}',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "hellip" => '\u{2026}',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "middot" => '\u{b7}',
        "trade" => '\u{2122}',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(html: &str) -> Vec<Piece<'_>> {
        pieces(html).collect()
    }

    /// The attributes `element` keeps from `attrs`, as the renderer receives them.
    fn values(element: &str, attrs: &[(&str, &str)]) -> Vec<(&'static str, String)> {
        let attrs: Vec<(String, &str)> = attrs
            .iter()
            .map(|(name, raw)| (name.to_ascii_lowercase(), *raw))
            .collect();
        attributes(element, &attrs, &mut HashSet::new())
    }

    #[test]
    fn tags_and_text_split_into_pieces() {
        assert_eq!(
            all("a <b x=\"1\" y>c</b>"),
            vec![
                Piece::Text("a "),
                Piece::Open {
                    name: "b",
                    attrs: vec![("x".to_owned(), "1"), ("y".to_owned(), "")],
                    raw: "<b x=\"1\" y>",
                },
                Piece::Text("c"),
                Piece::Close {
                    name: "b",
                    raw: "</b>",
                },
            ]
        );
    }

    #[test]
    fn what_is_not_a_complete_allowed_tag_stays_literal() {
        assert_eq!(
            all("<foo x=\"&amp;\"> <b"),
            vec![Piece::Literal("<foo x=\"&amp;\"> "), Piece::Literal("<b"),]
        );
        // A quoted value may not hold `<`, so this is not one tag.
        assert_eq!(
            all("<a title=\"x<y\">"),
            vec![Piece::Literal("<a title=\"x"), Piece::Literal("<y\">")]
        );
        assert_eq!(
            all("<img src=x/onerror=alert(1)>"),
            vec![Piece::Literal("<img src=x/onerror=alert(1)>")]
        );
    }

    #[test]
    fn comments_end_at_their_terminator_or_stay_literal() {
        assert_eq!(
            all("<!-- a <b> -->x"),
            vec![Piece::Comment, Piece::Text("x")]
        );
        assert_eq!(all("<!-- <b>"), vec![Piece::Literal("<!-- <b>")]);
        assert_eq!(all("<!-->"), vec![Piece::Literal("<!-->")]);
    }

    #[test]
    fn known_references_decode_and_unknown_ones_are_kept() {
        assert_eq!(
            decode_references("a&amp;b &#106;&#x6A; &copy; &#X6a;"),
            ("a&b jj \u{a9} j".to_owned(), false)
        );
        assert_eq!(
            decode_references("&unknown; &copy x &"),
            ("&unknown; &copy x &".to_owned(), true)
        );
        for bad in ["&#0;", "&#xD800;", "&#1114112;", "&#99999999999999;"] {
            assert!(decode_references(bad).1, "{bad}");
        }
        assert_eq!(decode_references("&#;"), ("&#;".to_owned(), false));
    }

    #[test]
    fn element_context_and_void_elements_are_fixed() {
        assert_eq!(element("div"), Some("div"));
        assert_eq!(element("script"), None);
        assert_eq!(element("input"), None);
        assert!(is_void("img") && is_void("source") && !is_void("div"));
        assert!(may_nest("li", Some("ul")) && !may_nest("li", Some("p")));
        assert!(!may_nest("li", None));
        assert!(may_nest("td", Some("tr")) && !may_nest("td", Some("table")));
        assert!(may_nest("source", Some("picture")) && !may_nest("source", Some("p")));
    }

    #[test]
    fn unsafe_or_unknown_urls_drop_the_attribute() {
        for href in [
            "&#106;avascript:x",
            "javascript&#58;x",
            "java&Tab;script:x",
            "x&unknown;y",
            "\\\\evil.test",
            "//evil.test",
        ] {
            assert!(values("a", &[("href", href)]).is_empty(), "{href}");
        }
        assert_eq!(
            values("a", &[("href", "https://x.test/?a=1&amp;b=2")]),
            vec![("href", "https://x.test/?a=1&b=2".to_owned())]
        );
    }

    #[test]
    fn a_source_keeps_every_safe_candidate_with_its_descriptor() {
        assert_eq!(
            values("source", &[("srcset", "a.png 1x,b.png 2x, c.png 100w")]),
            vec![("srcset", "a.png 1x, b.png 2x, c.png 100w".to_owned())]
        );
        assert!(values("source", &[("srcset", "ok.png 1x, javascript:x 2x")]).is_empty());
        assert!(values("source", &[("srcset", "a.png 1 x")]).is_empty());
        assert!(values("source", &[("srcset", "a.png 2y")]).is_empty());
        assert!(values("source", &[("srcset", "a.png,")]).is_empty());
    }

    #[test]
    fn a_source_keeps_at_most_sixteen_candidates() {
        let sixteen = (0..16)
            .map(|n| format!("{n}.png {}x", n + 1))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            values("source", &[("srcset", sixteen.as_str())]),
            vec![("srcset", sixteen.clone())]
        );
        // A seventeenth candidate drops the whole attribute, not just itself.
        let seventeen = format!("{sixteen}, 16.png 17x");
        assert!(values("source", &[("srcset", seventeen.as_str())]).is_empty());
        // Every candidate is safe, so only the count drops this long list.
        let many = "x,".repeat(250_000);
        assert!(values("source", &[("srcset", many.as_str())]).is_empty());
    }

    #[test]
    fn values_are_checked_for_their_element() {
        assert_eq!(values("td", &[("colspan", "12")]).len(), 1);
        for span in ["0", "1000", "x"] {
            assert!(values("td", &[("colspan", span)]).is_empty(), "{span}");
        }
        assert_eq!(values("img", &[("width", "100%")]).len(), 1);
        assert!(values("img", &[("width", "12345")]).is_empty());
        assert!(values("p", &[("src", "x.png")]).is_empty());
        assert_eq!(
            values("p", &[("align", "left"), ("onclick", "x"), ("class", "x")]).len(),
            1
        );
        assert!(values("img", &[("align", "justify")]).is_empty());
        assert_eq!(values("p", &[("align", "justify")]).len(), 1);
        assert_eq!(values("p", &[("dir", "rtl"), ("lang", "en-US")]).len(), 2);
        assert!(values("p", &[("lang", "en US")]).is_empty());
        assert!(values("p", &[("media", "x")]).is_empty());
    }

    #[test]
    fn the_first_attribute_of_a_name_wins_and_ids_are_not_reused() {
        assert_eq!(
            values("p", &[("title", "a"), ("title", "b")]),
            vec![("title", "a".to_owned())]
        );
        let mut ids = HashSet::new();
        let first = attributes("a", &[("name".to_owned(), "Top")], &mut ids);
        assert_eq!(first, vec![("id", "user-content-top".to_owned())]);
        assert!(attributes("p", &[("id".to_owned(), "top")], &mut ids).is_empty());
        assert!(attributes("p", &[("id".to_owned(), "!!!")], &mut ids).is_empty());
    }

    #[test]
    fn addresses_come_first_and_the_open_attribute_has_no_value() {
        let kept = values("img", &[("alt", "x"), ("src", "a.png")]);
        assert_eq!(kept[0].0, "src");
        assert_eq!(
            values("details", &[("OPEN", "")]),
            vec![("open", String::new())]
        );
    }

    #[test]
    fn the_tokenizer_and_references_stay_linear() {
        for unit in [
            "<!--",
            "<a x=\"",
            "<a ",
            "<a x='",
            "<a x=",
            "<a title=\"x<",
            "&#",
            "&amp",
            "&#x",
            "<b c=\"d\" ",
        ] {
            let time = |count: usize| {
                let source = unit.repeat(count);
                (0..3)
                    .map(|_| {
                        let started = std::time::Instant::now();
                        let _ = pieces(&source).count();
                        let _ = decode_references(&source);
                        started.elapsed()
                    })
                    .min()
                    .unwrap()
            };
            let small = time(8_000).max(std::time::Duration::from_millis(20));
            let large = time(32_000);
            assert!(
                large < small * 10 && large < std::time::Duration::from_secs(8),
                "{unit:?}: {small:?} then {large:?}"
            );
        }
    }
}
