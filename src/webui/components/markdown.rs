//! Markdown rendering: pulldown-cmark parses (CommonMark, tables, task lists, strikethrough,
//! alerts, footnotes in documents) and `Emitter` writes the HTML. Raw HTML in the source is
//! rendered only for an allow-list of elements and attributes: any other tag is shown as text,
//! a rejected attribute is removed, and URLs admit only explicitly safe schemes.

use super::code::themed_highlight;
use super::markdown_html::{attributes, decode_references, is_void, may_nest, pieces, Piece};
use super::references::{self, Candidates, Link, References};
use dioxus::prelude::*;
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, CowStr, Event, HeadingLevel, LinkType, OffsetIter,
    Options, Parser, Tag, TagEnd,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use unicase::UniCase;

// Source limits do not bound generated tags or repository-relative URL expansion.
const MAX_MARKDOWN_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_PLAIN_TEXT_BYTES: usize = 64 * 1024;
const MAX_NESTING: usize = 32;
// Markdown containers and raw HTML elements open at once, so a page cannot nest without limit.
const MAX_OPEN_ELEMENTS: usize = 64;
const MAX_EMOJI_NAME: usize = 32;
// Reference links per document. Past this, references stay plain text.
const MAX_REFERENCE_LINKS: usize = 1000;
// Highlighting costs far more than escaping, so a page pays for only so much of it.
const MAX_HIGHLIGHTED_BLOCK_BYTES: usize = 64 * 1024;
const MAX_HIGHLIGHTED_BYTES: usize = 256 * 1024;

/// Appends generated HTML, or returns `None` when it would exceed the output budget.
fn append_html(out: &mut String, text: &str) -> Option<()> {
    if text.len() > MAX_MARKDOWN_BYTES - out.len() {
        return None;
    }
    out.push_str(text);
    Some(())
}

/// The preview shown in place of Markdown whose rendered output is over budget.
fn plain_text_preview(text: &str) -> String {
    let mut end = text.len().min(MAX_PLAIN_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "<p>Markdown output is too large; showing a plain-text preview.</p>{}",
        code_block(&escape_html(&text[..end]))
    )
}

/// Escape the five HTML-significant characters (order-independent single pass).
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn code_block(code: &str) -> String {
    format!(
        "<pre class=\"my-4 overflow-x-auto rounded-md border border-border bg-muted/40 p-3\"><code class=\"font-mono text-sm text-foreground\">{code}</code></pre>"
    )
}

/// Whether a link or image may point at `url`: http(s), relative paths and, for links only,
/// mailto. The scheme is what precedes a colon that comes before the first `/`, `?` or `#`.
pub(super) fn safe_url(url: &str, image: bool) -> bool {
    if url.is_empty()
        || url.starts_with("//")
        || url
            .chars()
            .any(|c| (c.is_whitespace() && c != ' ') || c.is_control() || c == '\\')
    {
        return false;
    }
    let boundary = url.find(['/', '?', '#']).unwrap_or(url.len());
    let Some(colon) = url[..boundary].find(':') else {
        return true;
    };
    let scheme = &url[..colon];
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        return url[colon + 1..].starts_with("//");
    }
    !image && scheme.eq_ignore_ascii_case("mailto")
}

/// The anchor of a heading: the slug of its visible text, as on GitHub. A repeat gets `-1`,
/// `-2` and so on, skipping any id the page already has. The `user-content-` prefix keeps a
/// heading from taking the id of an element of the page, and links to `#name` match it.
fn heading_id(
    text: &str,
    emitted: &mut HashSet<String>,
    next_repeat: &mut HashMap<String, usize>,
) -> Option<String> {
    let slug = fragment_slug(text);
    if slug.is_empty() {
        return None;
    }
    // Ids are never removed, so every repeat below the stored one is still taken. Starting
    // there picks the same id as starting from zero, without walking the taken ones again.
    let mut repeat = next_repeat.get(&slug).copied().unwrap_or(0);
    let id = loop {
        let candidate = if repeat == 0 {
            slug.clone()
        } else {
            format!("{slug}-{repeat}")
        };
        if emitted.insert(candidate.clone()) {
            break candidate;
        }
        repeat += 1;
    };
    next_repeat.insert(slug, repeat + 1);
    Some(format!("user-content-{id}"))
}

/// The id of the heading a fragment names. An id that already has the prefix, and the top of
/// the page, are kept as they are.
fn heading_target(fragment: &str) -> String {
    if fragment.starts_with("user-content-") || fragment.is_empty() {
        fragment.to_owned()
    } else {
        format!(
            "user-content-{}",
            fragment_slug(&crate::webui::decode_component(fragment))
        )
    }
}

/// A fragment on a discussion page. Comments have their own `comment-<number>` ids, which the
/// native comment redirect uses too, so such a link keeps its id; any other fragment names a
/// heading.
fn discussion_target(fragment: &str) -> String {
    let comment = fragment
        .strip_prefix("comment-")
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()));
    if comment {
        fragment.to_owned()
    } else {
        heading_target(fragment)
    }
}

/// The `href` of a link to `url`, or `None` when the link is not safe. A fragment names a
/// heading or a comment, as `discussion_target` maps it.
pub(super) fn link_href(url: &str) -> Option<String> {
    if !safe_url(url, false) {
        return None;
    }
    Some(match url.strip_prefix('#') {
        Some(fragment) => format!("#{}", discussion_target(fragment)),
        None => url.replace(' ', "%20"),
    })
}

pub(super) fn fragment_slug(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            ' ' => Some("-".to_owned()),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c.to_lowercase().collect()),
            _ => None,
        })
        .collect()
}

/// The next `http://` or `https://` URL in `text` at or after `from`, as a byte range. It
/// starts a word, ends at whitespace or `<`, and gives back trailing punctuation and
/// unmatched `)`. Each search resumes where the last one ended, so the scan is linear.
fn next_bare_url(text: &str, from: usize) -> Option<std::ops::Range<usize>> {
    let mut at = from;
    while let Some(found) = text[at..].find("http") {
        let start = at + found;
        at = start + 4;
        let rest = &text[start..];
        let scheme = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            continue;
        };
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let mut end = start
            + rest
                .find(|c: char| c.is_whitespace() || c == '<')
                .unwrap_or(rest.len());
        at = end;
        let url = &text[start..end];
        let open = url.matches('(').count();
        let mut close = url.matches(')').count();
        for c in url.chars().rev() {
            match c {
                ')' if close > open => close -= 1,
                '?' | '!' | '.' | ',' | ':' | ';' | '*' | '_' | '~' | '\'' | '"' => {}
                _ => break,
            }
            end -= c.len_utf8();
        }
        if end - start > scheme {
            return Some(start..end);
        }
    }
    None
}

/// GitHub's most used emoji shortcodes, sorted by name for binary search.
const EMOJI: &[(&str, &str)] = &[
    ("+1", "👍"),
    ("-1", "👎"),
    ("100", "💯"),
    ("1234", "🔢"),
    ("airplane", "✈\u{FE0F}"),
    ("alarm_clock", "⏰"),
    ("alien", "👾"),
    ("angry", "😠"),
    ("apple", "🍎"),
    ("arrow_down", "⬇\u{FE0F}"),
    ("arrow_left", "⬅\u{FE0F}"),
    ("arrow_right", "➡\u{FE0F}"),
    ("arrow_up", "⬆\u{FE0F}"),
    ("art", "🎨"),
    ("astonished", "😲"),
    ("baby", "👶"),
    ("bangbang", "‼\u{FE0F}"),
    ("beer", "🍺"),
    ("beers", "🍻"),
    ("bell", "🔔"),
    ("bicyclist", "🚴"),
    ("bird", "🐦"),
    ("birthday", "🎂"),
    ("blue_heart", "💙"),
    ("bomb", "💣"),
    ("book", "📖"),
    ("bookmark", "🔖"),
    ("books", "📚"),
    ("boom", "💥"),
    ("bow", "🙇"),
    ("brain", "🧠"),
    ("broken_heart", "💔"),
    ("bug", "🐛"),
    ("bulb", "💡"),
    ("calendar", "📆"),
    ("camera", "📷"),
    ("cat", "🐱"),
    ("chart_with_upwards_trend", "📈"),
    ("checkered_flag", "🏁"),
    ("cherries", "🍒"),
    ("clap", "👏"),
    ("clipboard", "📋"),
    ("clock1", "🕐"),
    ("cloud", "☁\u{FE0F}"),
    ("coffee", "☕"),
    ("cold_sweat", "😰"),
    ("computer", "💻"),
    ("confetti_ball", "🎊"),
    ("confounded", "😖"),
    ("confused", "😕"),
    ("construction", "🚧"),
    ("construction_worker", "👷"),
    ("cookie", "🍪"),
    ("cool", "🆒"),
    ("copyright", "©\u{FE0F}"),
    ("crab", "🦀"),
    ("crossed_fingers", "🤞"),
    ("cry", "😢"),
    ("crystal_ball", "🔮"),
    ("dancer", "💃"),
    ("dart", "🎯"),
    ("deciduous_tree", "🌳"),
    ("disappointed", "😞"),
    ("dizzy", "💫"),
    ("dizzy_face", "😵"),
    ("dog", "🐶"),
    ("dolphin", "🐬"),
    ("door", "🚪"),
    ("dragon", "🐉"),
    ("droplet", "💧"),
    ("earth_africa", "🌍"),
    ("earth_americas", "🌎"),
    ("earth_asia", "🌏"),
    ("egg", "🥚"),
    ("elephant", "🐘"),
    ("email", "📧"),
    ("envelope", "✉\u{FE0F}"),
    ("exclamation", "❗"),
    ("expressionless", "😑"),
    ("eyes", "👀"),
    ("facepalm", "🤦"),
    ("fire", "🔥"),
    ("fireworks", "🎆"),
    ("first_place_medal", "🥇"),
    ("fist", "✊"),
    ("flags", "🎏"),
    ("flashlight", "🔦"),
    ("floppy_disk", "💾"),
    ("flushed", "😳"),
    ("folder", "📁"),
    ("fork_and_knife", "🍴"),
    ("four_leaf_clover", "🍀"),
    ("fox_face", "🦊"),
    ("free", "🆓"),
    ("fries", "🍟"),
    ("frowning", "😦"),
    ("gem", "💎"),
    ("gift", "🎁"),
    ("globe_with_meridians", "🌐"),
    ("grimacing", "😬"),
    ("grin", "😁"),
    ("grinning", "😀"),
    ("hammer", "🔨"),
    ("hammer_and_wrench", "🛠\u{FE0F}"),
    ("hand", "✋"),
    ("handshake", "🤝"),
    ("hankey", "💩"),
    ("heart", "❤\u{FE0F}"),
    ("heart_eyes", "😍"),
    ("heartbeat", "💓"),
    ("heavy_check_mark", "✔\u{FE0F}"),
    ("heavy_minus_sign", "➖"),
    ("heavy_plus_sign", "➕"),
    ("hourglass", "⌛"),
    ("hourglass_flowing_sand", "⏳"),
    ("house", "🏠"),
    ("hugs", "🤗"),
    ("hushed", "😯"),
    ("ice_cream", "🍨"),
    ("information_source", "ℹ\u{FE0F}"),
    ("innocent", "😇"),
    ("joy", "😂"),
    ("key", "🔑"),
    ("keyboard", "⌨\u{FE0F}"),
    ("kiss", "💋"),
    ("kissing_heart", "😘"),
    ("laughing", "😆"),
    ("leaves", "🍃"),
    ("lemon", "🍋"),
    ("link", "🔗"),
    ("lock", "🔒"),
    ("lollipop", "🍭"),
    ("loudspeaker", "📢"),
    ("mag", "🔍"),
    ("mag_right", "🔎"),
    ("mailbox", "📫"),
    ("memo", "📝"),
    ("metal", "🤘"),
    ("microphone", "🎤"),
    ("moneybag", "💰"),
    ("monkey", "🐒"),
    ("moon", "🌙"),
    ("mortar_board", "🎓"),
    ("muscle", "💪"),
    ("musical_note", "🎵"),
    ("nail_care", "💅"),
    ("neutral_face", "😐"),
    ("new", "🆕"),
    ("no_entry", "⛔"),
    ("no_entry_sign", "🚫"),
    ("nose", "👃"),
    ("notes", "🎶"),
    ("ok", "🆗"),
    ("ok_hand", "👌"),
    ("open_mouth", "😮"),
    ("package", "📦"),
    ("page_facing_up", "📄"),
    ("paperclip", "📎"),
    ("partly_sunny", "⛅"),
    ("pencil", "✏\u{FE0F}"),
    ("pencil2", "✏\u{FE0F}"),
    ("penguin", "🐧"),
    ("pensive", "😔"),
    ("pizza", "🍕"),
    ("point_down", "👇"),
    ("point_left", "👈"),
    ("point_right", "👉"),
    ("point_up", "☝\u{FE0F}"),
    ("poop", "💩"),
    ("pray", "🙏"),
    ("pushpin", "📌"),
    ("question", "❓"),
    ("rabbit", "🐰"),
    ("rage", "😡"),
    ("rainbow", "🌈"),
    ("raised_hands", "🙌"),
    ("recycle", "♻\u{FE0F}"),
    ("red_circle", "🔴"),
    ("relaxed", "☺\u{FE0F}"),
    ("relieved", "😌"),
    ("rocket", "🚀"),
    ("rose", "🌹"),
    ("rotating_light", "🚨"),
    ("runner", "🏃"),
    ("scream", "😱"),
    ("see_no_evil", "🙈"),
    ("seedling", "🌱"),
    ("shield", "🛡\u{FE0F}"),
    ("shrug", "🤷"),
    ("skull", "💀"),
    ("sleeping", "😴"),
    ("slightly_smiling_face", "🙂"),
    ("smile", "😄"),
    ("smiley", "😃"),
    ("smirk", "😏"),
    ("snake", "🐍"),
    ("snowflake", "❄\u{FE0F}"),
    ("sob", "😭"),
    ("soon", "🔜"),
    ("sparkles", "✨"),
    ("sparkling_heart", "💖"),
    ("speech_balloon", "💬"),
    ("star", "⭐"),
    ("star2", "🌟"),
    ("stop_sign", "🛑"),
    ("stuck_out_tongue", "😛"),
    ("stuck_out_tongue_winking_eye", "😜"),
    ("sun_with_face", "🌞"),
    ("sunglasses", "😎"),
    ("sunny", "☀\u{FE0F}"),
    ("sweat", "😓"),
    ("sweat_smile", "😅"),
    ("tada", "🎉"),
    ("tea", "🍵"),
    ("thinking", "🤔"),
    ("thumbsdown", "👎"),
    ("thumbsup", "👍"),
    ("tm", "™\u{FE0F}"),
    ("tongue", "👅"),
    ("trophy", "🏆"),
    ("turtle", "🐢"),
    ("twisted_rightwards_arrows", "🔀"),
    ("umbrella", "☂\u{FE0F}"),
    ("unamused", "😒"),
    ("unlock", "🔓"),
    ("v", "✌\u{FE0F}"),
    ("warning", "⚠\u{FE0F}"),
    ("wastebasket", "🗑\u{FE0F}"),
    ("wave", "👋"),
    ("white_check_mark", "✅"),
    ("white_circle", "⚪"),
    ("wink", "😉"),
    ("wolf", "🐺"),
    ("worried", "😟"),
    ("wrench", "🔧"),
    ("x", "❌"),
    ("yellow_heart", "💛"),
    ("yum", "😋"),
    ("zap", "⚡"),
    ("zzz", "💤"),
];

/// The character for the shortcode that `:name:` starts at `colon`, and how many bytes the
/// shortcode takes. Names are short, so each colon costs a bounded look.
fn emoji_at(text: &str, colon: usize) -> Option<(usize, &'static str)> {
    let rest = &text[colon + 1..];
    let length = rest
        .bytes()
        .take(MAX_EMOJI_NAME + 1)
        .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_+-".contains(b))
        .count();
    if length == 0 || length > MAX_EMOJI_NAME || rest.as_bytes().get(length) != Some(&b':') {
        return None;
    }
    let name = &rest[..length];
    let found = EMOJI.binary_search_by_key(&name, |(name, _)| name).ok()?;
    Some((length + 2, EMOJI[found].1))
}

/// An entry on the stack of open elements. A Markdown container is closed by its own end, which
/// first closes the raw HTML elements opened inside it. A raw HTML element is closed by its end
/// tag, or by the end of its container.
#[derive(Clone, Copy)]
enum Open {
    Markdown(&'static str),
    User(&'static str),
}

impl Open {
    fn name(&self) -> &'static str {
        match *self {
            Open::Markdown(name) | Open::User(name) => name,
        }
    }
}

#[derive(Clone, Copy)]
struct Heading {
    level: HeadingLevel,
    /// Where the heading's inner HTML starts in the output.
    start: usize,
}

/// Writes events as HTML. Tight list items and table cells have no paragraphs, as in
/// CommonMark; everything else follows the tags the parser sends.
struct Emitter<'r> {
    out: String,
    /// A document (a repository file) keeps footnotes and joins soft line breaks; comments
    /// show each source line break.
    document: bool,
    nesting: usize,
    heading: Option<Heading>,
    /// The visible text of the open heading, which its id is made from.
    heading_text: String,
    heading_ids: HashSet<String>,
    next_repeat: HashMap<String, usize>,
    /// `Some(written)` inside a link; a link with an unsafe URL is not written.
    link: Option<bool>,
    table: Vec<Alignment>,
    in_table_head: bool,
    column: usize,
    /// Numbered by first use. The parser matches a reference to its definition with Unicode
    /// case folding, so the key folds too.
    footnotes: HashMap<UniCase<String>, usize>,
    /// Innermost last.
    open: Vec<Open>,
    highlighted: usize,
    references: &'r References,
    /// Reference links written so far, up to `MAX_REFERENCE_LINKS`.
    reference_links: usize,
    /// Set when the emitter only collects the references a page could link, see
    /// `collect_candidates`.
    found: Option<&'r mut Candidates>,
    /// The form that task boxes submit. Without it, boxes are read-only checkboxes.
    task_form: Option<&'r str>,
    /// Task list items written so far; the item number is the box's value.
    tasks: usize,
}

impl<'r> Emitter<'r> {
    fn new(document: bool, references: &'r References, task_form: Option<&'r str>) -> Self {
        Self {
            out: String::new(),
            document,
            nesting: 0,
            heading: None,
            heading_text: String::new(),
            heading_ids: HashSet::new(),
            next_repeat: HashMap::new(),
            link: None,
            table: Vec::new(),
            in_table_head: false,
            column: 0,
            footnotes: HashMap::new(),
            open: Vec::new(),
            highlighted: 0,
            references,
            reference_links: 0,
            found: None,
            task_form,
            tasks: 0,
        }
    }

    fn write(&mut self, html: &str) -> Option<()> {
        // A candidate walk keeps no output. Only the structure it reads matters, and the output
        // budget must not end the walk early.
        if self.found.is_some() {
            return Some(());
        }
        append_html(&mut self.out, html)
    }

    fn newline(&mut self) -> Option<()> {
        if self.out.is_empty() || self.out.ends_with('\n') {
            return Some(());
        }
        self.write("\n")
    }

    /// Writes a block-level tag on a line of its own.
    fn block(&mut self, html: &str) -> Option<()> {
        self.newline()?;
        self.write(html)
    }

    /// Text a reader sees, for the id of the open heading.
    fn visible(&mut self, text: &str) {
        if self.heading.is_some() {
            self.heading_text.push_str(text);
        }
    }

    /// Raw HTML from a source block or an inline event, written as its allowed elements and text.
    fn html(&mut self, source: &str) -> Option<()> {
        for piece in pieces(source) {
            match piece {
                Piece::Text(text) => self.write(&escape_html(&decode_references(text).0))?,
                Piece::Literal(text) => self.literal(text)?,
                Piece::Comment => {}
                Piece::Open { name, attrs, raw } => self.open_user(name, &attrs, raw)?,
                Piece::Close { name, raw } => self.close_user(name, raw)?,
            }
        }
        Some(())
    }

    /// An HTML block is read to its end, so a tag split across its lines is one tag.
    fn html_block<'a>(&mut self, events: &mut impl Iterator<Item = Event<'a>>) -> Option<()> {
        let mut source = String::new();
        for event in events {
            match event {
                Event::Html(text) => source.push_str(&text),
                Event::End(TagEnd::HtmlBlock) => break,
                _ => {}
            }
        }
        self.newline()?;
        self.html(&source)
    }

    /// Source shown as text: a tag that is not allowed here, or a `<` that starts no tag.
    fn literal(&mut self, source: &str) -> Option<()> {
        self.visible(source);
        self.write(&escape_html(source))
    }

    /// An allowed element opens, unless it is out of place, a link inside a link, or nested
    /// too deep. Otherwise its source is shown as text.
    fn open_user(&mut self, name: &'static str, attrs: &[(String, &str)], raw: &str) -> Option<()> {
        let parent = self.open.last().map(Open::name);
        // The stack size is checked before the link scan, which walks the whole stack.
        if self.open.len() >= MAX_OPEN_ELEMENTS
            || !may_nest(name, parent)
            || (name == "a" && self.open.iter().any(|open| open.name() == "a"))
        {
            return self.literal(raw);
        }
        // A candidate walk keeps no ids, so its uniqueness check uses a set that is thrown away.
        let mut scratch = HashSet::new();
        let ids = if self.found.is_some() {
            &mut scratch
        } else {
            &mut self.heading_ids
        };
        let values = attributes(name, attrs, ids);
        let html = match name {
            "a" => {
                let title = value(&values, "title").unwrap_or("");
                let id = value(&values, "id").unwrap_or("");
                match value(&values, "href") {
                    Some(href) => anchor(href, title, id),
                    None => format!("<a{}{}>", title_attribute(title), id_attribute(id)),
                }
            }
            "img" => {
                let alt = value(&values, "alt").unwrap_or("");
                let Some(src) = value(&values, "src") else {
                    // An image without a usable address shows its alt text and writes no id.
                    release_id(&values, &mut self.heading_ids);
                    self.visible(alt);
                    return self.write(&escape_html(alt));
                };
                image_tag(src, alt, &pairs(&values, &["src", "alt"]))
            }
            "source" => {
                let Some(srcset) = value(&values, "srcset") else {
                    // A source without a usable address is dropped, and the image shows.
                    release_id(&values, &mut self.heading_ids);
                    return Some(());
                };
                format!(
                    "<source srcset=\"{}\"{}>",
                    escape_html(srcset),
                    pairs(&values, &["srcset"])
                )
            }
            _ => format!("<{name}{}>", pairs(&values, &[])),
        };
        if !is_void(name) {
            self.open.push(Open::User(name));
        }
        self.write(&html)
    }

    /// An end tag closes the element it matches, when that element is open in the same Markdown
    /// container, together with the elements opened after it. Otherwise it is shown as text.
    fn close_user(&mut self, name: &str, raw: &str) -> Option<()> {
        let depth = self
            .open
            .iter()
            .rev()
            .take_while(|open| matches!(open, Open::User(_)))
            .position(|open| open.name() == name);
        let Some(depth) = depth else {
            return self.literal(raw);
        };
        for _ in 0..=depth {
            if let Some(Open::User(closed)) = self.open.pop() {
                self.write(&format!("</{closed}>"))?;
            }
        }
        Some(())
    }

    /// Starts a Markdown container, which `leave` ends. Past `MAX_NESTING` the render stops and
    /// the page shows its preview.
    fn enter(&mut self, name: &'static str) -> Option<()> {
        if counts_toward_nesting(name) {
            self.nest()?;
        }
        self.open.push(Open::Markdown(name));
        Some(())
    }

    /// Ends the innermost Markdown container: first the raw HTML elements left open inside it,
    /// then the container itself.
    fn leave(&mut self) -> Option<()> {
        self.close_user_elements()?;
        if let Some(Open::Markdown(name)) = self.open.pop() {
            if counts_toward_nesting(name) {
                self.nesting -= 1;
            }
        }
        Some(())
    }

    /// Writes the end tags of the raw HTML elements on top of the stack.
    fn close_user_elements(&mut self) -> Option<()> {
        while let Some(Open::User(name)) = self.open.last().copied() {
            self.open.pop();
            self.write(&format!("</{name}>"))?;
        }
        Some(())
    }

    fn nest(&mut self) -> Option<()> {
        self.nesting += 1;
        (self.nesting <= MAX_NESTING).then_some(())
    }

    fn event<'a>(&mut self, event: Event<'a>, events: &mut TextRuns<'a>) -> Option<()> {
        match event {
            Event::Start(tag) => self.start(tag, events),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.text(&text, events.opaque())
            }
            Event::Code(code) => {
                self.visible(&code);
                self.write(&format!(
                    "<code class=\"rounded border border-border bg-muted px-1 py-0.5 text-[0.85em] text-foreground\">{}</code>",
                    escape_html(&code)
                ))
            }
            Event::Html(html) | Event::InlineHtml(html) => self.html(&html),
            Event::SoftBreak => {
                self.visible(" ");
                self.write(if self.document { " " } else { "<br>" })
            }
            Event::HardBreak => {
                self.visible(" ");
                self.write("<br>")
            }
            Event::Rule => self.block("<hr class=\"my-6 border-border\">"),
            Event::FootnoteReference(label) => {
                let number = self.footnote_number(&label);
                self.write(&format!(
                    "<sup class=\"footnote-reference\"><a href=\"#user-content-fn:{number}\">{number}</a></sup>"
                ))
            }
            Event::TaskListMarker(checked) => {
                let index = self.tasks;
                self.tasks += 1;
                match self.task_form {
                    Some(form) => {
                        let label = if checked {
                            "Mark as not done"
                        } else {
                            "Mark as done"
                        };
                        self.write(&format!(
                            "<button type=\"submit\" form=\"{}\" name=\"task\" value=\"{index}\" class=\"task-box{}\" aria-label=\"{label}\" title=\"{label}\"></button>",
                            escape_html(form),
                            if checked { " task-box-done" } else { "" }
                        ))
                    }
                    None => self.write(&format!(
                        "<input type=\"checkbox\" disabled{} class=\"mr-2\">",
                        if checked { " checked" } else { "" }
                    )),
                }
            }
        }
    }

    fn start<'a>(
        &mut self,
        tag: Tag<'a>,
        events: &mut impl Iterator<Item = Event<'a>>,
    ) -> Option<()> {
        match tag {
            Tag::Paragraph => {
                self.enter("p")?;
                self.block("<p class=\"my-3 leading-7 text-foreground\">")
            }
            Tag::HtmlBlock => self.html_block(events),
            Tag::Heading { level, .. } => {
                self.enter(heading_tag(level).0)?;
                self.newline()?;
                self.heading = Some(Heading {
                    level,
                    start: self.out.len(),
                });
                self.heading_text.clear();
                Some(())
            }
            Tag::BlockQuote(kind) => {
                self.enter("blockquote")?;
                self.block(&blockquote_open(kind))
            }
            Tag::CodeBlock(kind) => self.code_block(&kind, events),
            Tag::List(first) => {
                self.enter(if first.is_some() { "ol" } else { "ul" })?;
                self.block(&match first {
                    None => "<ul class=\"my-3 list-disc space-y-1 pl-5 text-foreground\">".to_owned(),
                    Some(1) => "<ol class=\"my-3 list-decimal space-y-1 pl-5 text-foreground\">".to_owned(),
                    Some(start) => format!(
                        "<ol start=\"{start}\" class=\"my-3 list-decimal space-y-1 pl-5 text-foreground\">"
                    ),
                })
            }
            Tag::Item => {
                self.enter("li")?;
                self.block("<li>")
            }
            Tag::FootnoteDefinition(label) => {
                self.enter("div")?;
                let number = self.footnote_number(&label);
                self.block(&format!(
                    "<div class=\"footnote-definition\" id=\"user-content-fn:{number}\"><sup class=\"footnote-definition-label\">{number}</sup>"
                ))
            }
            Tag::Table(alignments) => {
                self.enter("table")?;
                self.table = alignments;
                self.block("<table>")
            }
            Tag::TableHead => {
                self.enter("thead")?;
                self.in_table_head = true;
                self.column = 0;
                self.block("<thead><tr>")
            }
            Tag::TableRow => {
                self.enter("tr")?;
                self.column = 0;
                self.block("<tr>")
            }
            Tag::TableCell => {
                self.enter(if self.in_table_head { "th" } else { "td" })?;
                let class = match self.table.get(self.column) {
                    Some(Alignment::Left) => " class=\"text-left\"",
                    Some(Alignment::Center) => " class=\"text-center\"",
                    Some(Alignment::Right) => " class=\"text-right\"",
                    _ => "",
                };
                self.column += 1;
                self.write(&format!(
                    "<{}{class}>",
                    if self.in_table_head { "th" } else { "td" }
                ))
            }
            Tag::Emphasis => {
                self.enter("em")?;
                self.write("<em class=\"italic\">")
            }
            Tag::Strong => {
                self.enter("strong")?;
                self.write("<strong class=\"font-semibold text-foreground\">")
            }
            Tag::Strikethrough => {
                self.enter("del")?;
                self.write("<del>")
            }
            Tag::Link {
                link_type,
                dest_url,
                title,
                ..
            } => {
                self.enter("a")?;
                self.link_start(link_type, &dest_url, &title)
            }
            Tag::Image {
                dest_url, title, ..
            } => self.image(&dest_url, &title, events),
            // Options that produce the other tags are not enabled.
            _ => Some(()),
        }
    }

    fn end(&mut self, tag: TagEnd) -> Option<()> {
        match tag {
            TagEnd::Paragraph => {
                self.leave()?;
                self.write("</p>")
            }
            TagEnd::Heading(_) => {
                self.leave()?;
                self.heading_end()
            }
            TagEnd::BlockQuote(_) => {
                self.leave()?;
                self.block("</blockquote>")
            }
            TagEnd::List(ordered) => {
                self.leave()?;
                self.block(if ordered { "</ol>" } else { "</ul>" })
            }
            TagEnd::Item => {
                self.leave()?;
                self.write("</li>")
            }
            TagEnd::FootnoteDefinition => {
                self.leave()?;
                self.write("</div>")
            }
            TagEnd::Table => {
                self.leave()?;
                self.block("</tbody></table>")
            }
            TagEnd::TableHead => {
                self.leave()?;
                self.in_table_head = false;
                self.write("</tr></thead>\n<tbody>")
            }
            TagEnd::TableRow => {
                self.leave()?;
                self.write("</tr>")
            }
            TagEnd::TableCell => {
                self.leave()?;
                self.write(if self.in_table_head { "</th>" } else { "</td>" })
            }
            TagEnd::Emphasis => {
                self.leave()?;
                self.write("</em>")
            }
            TagEnd::Strong => {
                self.leave()?;
                self.write("</strong>")
            }
            TagEnd::Strikethrough => {
                self.leave()?;
                self.write("</del>")
            }
            TagEnd::Link => {
                self.leave()?;
                match self.link.take() {
                    Some(true) => self.write("</a>"),
                    _ => Some(()),
                }
            }
            // Code blocks and images are read to their end where they start.
            _ => Some(()),
        }
    }

    /// Whether the text being written sits inside a Markdown link, or inside a raw `a`, `code` or
    /// `pre` element. Such text stays as written: a link inside a link, and a reference or a bare
    /// URL inside code, are never wanted.
    fn inside_link_or_code(&self) -> bool {
        self.link.is_some()
            || self
                .open
                .iter()
                .any(|open| matches!(open.name(), "a" | "code" | "pre"))
    }

    /// Plain text, with bare `http(s)://` URLs outside links made into links, known `:emoji:`
    /// shortcodes replaced and reference tokens linked. One pass: each search resumes where the
    /// last ended. `opaque` holds the bytes of `text` that the source does not show as written.
    fn text(&mut self, text: &str, opaque: &[Range<usize>]) -> Option<()> {
        self.visible(text);
        let mut url = if self.inside_link_or_code() {
            None
        } else {
            next_bare_url(text, 0)
        };
        let mut done = 0;
        let mut search = 0;
        loop {
            let colon = text[search..].find(':').map(|at| search + at);
            if let Some(range) = url.clone().filter(|r| colon.is_none_or(|c| r.start <= c)) {
                if safe_url(&text[range.clone()], false) {
                    self.plain(text, done..range.start, opaque)?;
                    self.write(&anchor(&text[range.clone()], "", ""))?;
                    self.write(&escape_html(&text[range.clone()]))?;
                    self.write("</a>")?;
                    done = range.end;
                }
                search = range.end;
                url = next_bare_url(text, range.end);
            } else if let Some(colon) = colon {
                search = colon + 1;
                let in_code = self
                    .open
                    .iter()
                    .any(|open| matches!(open.name(), "code" | "pre"));
                if let Some((length, emoji)) = emoji_at(text, colon).filter(|_| !in_code) {
                    self.plain(text, done..colon, opaque)?;
                    self.write(emoji)?;
                    done = colon + length;
                    search = done;
                }
            } else {
                break;
            }
        }
        self.plain(text, done..text.len(), opaque)
    }

    /// Writes plain text, with each reference token that links on this page made into a link.
    /// Text inside a link or a raw `a`, `code` or `pre` element, and text past the link budget,
    /// stays as it is. A candidate walk records the tokens of the text that would link instead.
    fn plain(&mut self, text: &str, range: Range<usize>, opaque: &[Range<usize>]) -> Option<()> {
        if self.inside_link_or_code() {
            return self.write(&escape_html(&text[range.clone()]));
        }
        if let Some(found) = self.found.as_deref_mut() {
            found.add_tokens(text, range.clone(), opaque);
            return self.write(&escape_html(&text[range]));
        }
        if self.references.repo.is_empty() {
            return self.write(&escape_html(&text[range.clone()]));
        }
        // Only the links are kept, and no more than the budget that is left, so a long run costs
        // what its links cost rather than what its tokens cost.
        let budget = MAX_REFERENCE_LINKS - self.reference_links;
        let mut links = Vec::new();
        references::scan_tokens(text, range.clone(), opaque, |token_range, token| {
            if links.len() < budget {
                if let Some(link) = self.references.link(&token) {
                    links.push((token_range, link));
                }
            }
        });
        let mut done = range.start;
        for (token_range, link) in links {
            self.reference_links += 1;
            self.write(&escape_html(&text[done..token_range.start]))?;
            self.write(&reference_anchor(&link))?;
            done = token_range.end;
        }
        self.write(&escape_html(&text[done..range.end]))
    }

    fn link_start(&mut self, link_type: LinkType, dest: &str, title: &str) -> Option<()> {
        let url = if link_type == LinkType::Email {
            format!("mailto:{dest}")
        } else {
            dest.to_owned()
        };
        let href = link_href(&url);
        self.link = Some(href.is_some());
        match href {
            Some(href) => self.write(&anchor(&href, title, "")),
            None => Some(()),
        }
    }

    /// An image reads its alt text from the events up to its end. An image with an unsafe
    /// URL is written as that text alone.
    fn image<'a>(
        &mut self,
        dest: &str,
        title: &str,
        events: &mut impl Iterator<Item = Event<'a>>,
    ) -> Option<()> {
        let mut alt = String::new();
        let mut depth = 1;
        for event in events {
            match event {
                Event::Start(Tag::Image { .. }) => depth += 1,
                Event::End(TagEnd::Image) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Text(text) | Event::Code(text) => alt.push_str(&text),
                Event::SoftBreak | Event::HardBreak => alt.push(' '),
                _ => {}
            }
        }
        self.visible(&alt);
        if !safe_url(dest, true) {
            return self.write(&escape_html(&alt));
        }
        self.write(&image_tag(
            &dest.replace(' ', "%20"),
            &alt,
            &title_attribute(title),
        ))
    }

    /// A code block's text is read to its end. Fenced blocks with a language are
    /// highlighted while the page is within its highlighting budget.
    fn code_block<'a>(
        &mut self,
        kind: &CodeBlockKind<'_>,
        events: &mut impl Iterator<Item = Event<'a>>,
    ) -> Option<()> {
        let mut code = String::new();
        for event in events {
            match event {
                // A candidate walk reads no code, so it keeps none.
                Event::Text(text) if self.found.is_none() => code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => break,
                _ => {}
            }
        }
        if self.found.is_some() {
            return Some(());
        }
        if code.ends_with('\n') {
            code.pop();
        }
        let language = match kind {
            CodeBlockKind::Fenced(info) => info
                .split(|c: char| c.is_whitespace() || c == ',')
                .next()
                .unwrap_or(""),
            CodeBlockKind::Indented => "",
        };
        // A candidate walk reads no highlighted markup, so it leaves the code escaped.
        let html = if self.found.is_none()
            && !language.is_empty()
            && code.len() <= MAX_HIGHLIGHTED_BLOCK_BYTES
            && code.len() <= MAX_HIGHLIGHTED_BYTES - self.highlighted
        {
            self.highlighted += code.len();
            themed_highlight(&code, language)
        } else {
            escape_html(&code)
        };
        self.block(&code_block(&html))
    }

    fn footnote_number(&mut self, label: &str) -> usize {
        // A candidate walk writes no footnotes, so it keeps no numbers.
        if self.found.is_some() {
            return 1;
        }
        let next = self.footnotes.len() + 1;
        *self
            .footnotes
            .entry(UniCase::new(label.to_owned()))
            .or_insert(next)
    }

    /// Replaces the heading's inner HTML, written in place, with the whole heading.
    fn heading_end(&mut self) -> Option<()> {
        let heading = self.heading.take()?;
        let inner = self.out.split_off(heading.start);
        let text = std::mem::take(&mut self.heading_text);
        // A candidate walk keeps no heading ids, and its output is discarded.
        if self.found.is_some() {
            return Some(());
        }
        let id = heading_id(&text, &mut self.heading_ids, &mut self.next_repeat);
        self.write_heading(heading.level, id.as_deref(), &inner)
    }

    fn write_heading(&mut self, level: HeadingLevel, id: Option<&str>, inner: &str) -> Option<()> {
        let (tag, class) = heading_tag(level);
        // A whole document's ids are unique on the page, so its headings get a permalink.
        // Comments share a page and repeat ids, so they get none. The link is empty: CSS draws
        // its glyph, so copied text is unchanged.
        let permalink = match id {
            Some(id) if self.document => format!(
                "<a class=\"heading-permalink\" href=\"#{id}\" aria-label=\"Permalink to this section\"></a>"
            ),
            _ => String::new(),
        };
        let id = id.map(|id| format!(" id=\"{id}\"")).unwrap_or_default();
        self.write(&format!(
            "<{tag}{id} class=\"{class}\">{permalink}{inner}</{tag}>"
        ))
    }
}

/// Whether a Markdown container counts toward `MAX_NESTING`. Paragraphs and list items do not,
/// so a list level costs one unit (its list) and the text inside it costs none.
fn counts_toward_nesting(name: &str) -> bool {
    !matches!(name, "p" | "li")
}

/// The tag and classes of a heading of `level`.
fn heading_tag(level: HeadingLevel) -> (&'static str, &'static str) {
    match level {
        HeadingLevel::H1 => (
            "h1",
            "mb-4 mt-6 border-b border-border pb-2 text-3xl font-bold text-foreground first:mt-0",
        ),
        HeadingLevel::H2 => (
            "h2",
            "mb-3 mt-5 border-b border-border pb-2 text-2xl font-semibold text-foreground",
        ),
        HeadingLevel::H3 => ("h3", "mb-2 mt-4 text-xl font-semibold text-foreground"),
        HeadingLevel::H4 => ("h4", "mb-2 mt-4 text-lg font-semibold text-foreground"),
        HeadingLevel::H5 => ("h5", "mb-2 mt-4 text-base font-semibold text-foreground"),
        HeadingLevel::H6 => (
            "h6",
            "mb-2 mt-4 text-sm font-semibold text-muted-foreground",
        ),
    }
}

/// The opening tag of a link that opens in a new tab; `LinkBase::rewrite` relies on `href`
/// being first and `NEW_TAB` following it.
fn anchor(href: &str, title: &str, id: &str) -> String {
    format!(
        "<a href=\"{}\"{NEW_TAB} class=\"text-primary hover:underline\"{}{}>",
        escape_html(href),
        title_attribute(title),
        id_attribute(id)
    )
}

fn id_attribute(id: &str) -> String {
    if id.is_empty() {
        String::new()
    } else {
        format!(" id=\"{}\"", escape_html(id))
    }
}

/// An image. `extra` holds the attributes its element had besides `src` and `alt`, as
/// ` name="value"` text.
fn image_tag(src: &str, alt: &str, extra: &str) -> String {
    format!(
        "<img src=\"{}\" alt=\"{}\"{extra} loading=\"lazy\" class=\"my-3 max-w-full rounded-md border border-border\" />",
        escape_html(src),
        escape_html(alt)
    )
}

/// The value of attribute `name`, if the element has it.
fn value<'a>(values: &'a [(&str, String)], name: &str) -> Option<&'a str> {
    values
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.as_str())
}

/// The attributes as ` name="value"` text, values escaped, leaving out the names in `skip`. A
/// boolean attribute has no value.
fn pairs(values: &[(&str, String)], skip: &[&str]) -> String {
    let mut out = String::new();
    for (name, value) in values {
        if skip.contains(name) {
            continue;
        }
        if *name == "open" {
            out.push_str(" open");
        } else {
            out.push_str(&format!(" {name}=\"{}\"", escape_html(value)));
        }
    }
    out
}

/// Gives back the `id` that `attributes` reserved for an element that is not written after all,
/// so a heading may still take it.
fn release_id(values: &[(&str, String)], ids: &mut HashSet<String>) {
    if let Some(slug) = value(values, "id").and_then(|id| id.strip_prefix("user-content-")) {
        ids.remove(slug);
    }
}

fn title_attribute(title: &str) -> String {
    if title.is_empty() {
        String::new()
    } else {
        format!(" title=\"{}\"", escape_html(title))
    }
}

fn blockquote_open(kind: Option<BlockQuoteKind>) -> String {
    let (name, title) = match kind {
        None => return "<blockquote>".to_owned(),
        Some(BlockQuoteKind::Note) => ("note", "Note"),
        Some(BlockQuoteKind::Tip) => ("tip", "Tip"),
        Some(BlockQuoteKind::Important) => ("important", "Important"),
        Some(BlockQuoteKind::Warning) => ("warning", "Warning"),
        Some(BlockQuoteKind::Caution) => ("caution", "Caution"),
    };
    format!(
        "<blockquote class=\"markdown-alert markdown-alert-{name}\"><p class=\"markdown-alert-title\">{title}</p>"
    )
}

/// Markdown for comments, descriptions and previews: every source line break shows.
pub fn parse_markdown(text: &str, references: &References) -> String {
    bounded_markdown(text, false, references).unwrap_or_else(|| plain_text_preview(text))
}

/// Markdown whose task boxes are buttons. Each button submits the form `task_form` with its
/// item number in `task`; the form must sit on the same page.
pub fn parse_markdown_with_tasks(text: &str, references: &References, task_form: &str) -> String {
    render_markdown(text, false, references, Some(task_form))
        .unwrap_or_else(|| plain_text_preview(text))
}

/// Markdown for a file in a repository: lines of a paragraph join, footnotes work.
fn parse_document(text: &str, references: &References) -> String {
    bounded_markdown(text, true, references).unwrap_or_else(|| plain_text_preview(text))
}

pub(super) fn markdown_options(document: bool) -> Options {
    let mut options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    if document {
        options |= Options::ENABLE_FOOTNOTES;
    }
    options
}

/// `None` when the source or its output is over budget, or nests deeper than `MAX_NESTING`.
fn bounded_markdown(text: &str, document: bool, references: &References) -> Option<String> {
    render_markdown(text, document, references, None)
}

fn render_markdown(
    text: &str,
    document: bool,
    references: &References,
    task_form: Option<&str>,
) -> Option<String> {
    if text.len() > MAX_SOURCE_BYTES {
        return None;
    }
    if text.trim().is_empty() {
        return Some(String::new());
    }
    let mut emitter = Emitter::new(document, references, task_form);
    let events = Parser::new_ext(text, markdown_options(document)).into_offset_iter();
    let mut events = TextRuns::new(text, events);
    while let Some(event) = events.next() {
        emitter.event(event, &mut events)?;
    }
    emitter.close_user_elements()?;
    Some(emitter.out)
}

/// Adds to `found` the references that `source` could link. The emitter walks the source as the
/// renderer does, so the raw elements, the containers and the opaque ranges of each text run are
/// the renderer's own, and each run yields the tokens the renderer finds in it. The walk keeps no
/// output, no heading ids, no footnote numbers and no code. Its state is the source, the current
/// text run with its opaque ranges, the open elements, the candidate lists (which are capped) and
/// the id set of one raw tag while that tag is read. The opaque ranges hold at most one 16-byte
/// range per text event, and the next run replaces them. The parser's events are the ones the
/// render reads.
pub(super) fn collect_candidates(source: &str, document: bool, found: &mut Candidates) {
    if source.len() > MAX_SOURCE_BYTES || source.trim().is_empty() {
        return;
    }
    let none = References::default();
    let mut emitter = Emitter::new(document, &none, None);
    emitter.found = Some(found);
    let events = Parser::new_ext(source, markdown_options(document)).into_offset_iter();
    let mut events = TextRuns::new(source, events);
    while let Some(event) = events.next() {
        if emitter.event(event, &mut events).is_none() {
            break;
        }
    }
}

/// The parser's events with adjacent text merged, as `TextMergeStream` merges it. Each merged run
/// also records the bytes that a reference may not use: text the source does not show as written,
/// such as a decoded entity, and the first character of a piece that follows a backslash or a gap
/// in the source, which is an escaped character.
struct TextRuns<'a> {
    source: &'a str,
    events: OffsetIter<'a>,
    /// The event that ended the last text run. The next call returns it.
    pending: Option<(Event<'a>, Range<usize>)>,
    /// The opaque ranges of the run last returned, in order.
    opaque_ranges: Vec<Range<usize>>,
}

impl<'a> TextRuns<'a> {
    fn new(source: &'a str, events: OffsetIter<'a>) -> Self {
        Self {
            source,
            events,
            pending: None,
            opaque_ranges: Vec::new(),
        }
    }

    /// The opaque ranges of the text run last returned by `next`.
    fn opaque(&self) -> &[Range<usize>] {
        &self.opaque_ranges
    }

    /// Records the opaque bytes of `piece`, which the source has at `range` and which starts at
    /// `start` in its run. `previous` is where the run's previous piece ended in the source.
    fn mark(&mut self, piece: &str, range: Range<usize>, start: usize, previous: Option<usize>) {
        if piece.len() != range.len() {
            self.opaque_ranges.push(start..start + piece.len());
            return;
        }
        let gap = previous.is_some_and(|end| end != range.start);
        let escaped = range.start > 0 && self.source.as_bytes()[range.start - 1] == b'\\';
        let first = piece.chars().next().map_or(0, char::len_utf8);
        if (gap || escaped) && first > 0 {
            self.opaque_ranges.push(start..start + first);
        }
    }
}

impl<'a> Iterator for TextRuns<'a> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        self.opaque_ranges.clear();
        let (event, range) = match self.pending.take() {
            Some(pending) => pending,
            None => self.events.next()?,
        };
        let first = match event {
            Event::Text(first) => first,
            other => return Some(other),
        };
        self.mark(&first, range.clone(), 0, None);
        let mut end = range.end;
        let mut joined: Option<String> = None;
        loop {
            match self.events.next() {
                Some((Event::Text(piece), piece_range)) => {
                    let buffer = joined.get_or_insert_with(|| first.to_string());
                    let start = buffer.len();
                    buffer.push_str(&piece);
                    self.mark(&piece, piece_range.clone(), start, Some(end));
                    end = piece_range.end;
                }
                other => {
                    self.pending = other;
                    break;
                }
            }
        }
        Some(Event::Text(match joined {
            Some(buffer) => CowStr::from(buffer),
            None => first,
        }))
    }
}

/// The anchor of a linked reference. It points inside the forge, so it opens in the same tab.
fn reference_anchor(link: &Link) -> String {
    format!(
        "<a href=\"{}\" class=\"{}\"{}>{}</a>",
        escape_html(&link.href),
        link.class,
        title_attribute(link.title.as_deref().unwrap_or("")),
        escape_html(&link.text)
    )
}

/// Renders Markdown as HTML. The references name what the page may link; the default value
/// links nothing.
#[component]
pub fn Markdown(
    content: String,
    #[props(default)] references: References,
    // The form the task boxes submit, when the viewer may tick them.
    #[props(default)] task_form: Option<String>,
) -> Element {
    let html = match &task_form {
        Some(form) => parse_markdown_with_tasks(&content, &references, form),
        None => parse_markdown(&content, &references),
    };

    rsx! {
        div { class: "markdown-body leading-relaxed text-foreground", dangerous_inner_html: "{html}" }
    }
}

/// Where a repository file's relative links point: the `blob`, `tree` and `raw` URLs of
/// the same ref, and the directory the file is in (no leading or trailing slash). The default
/// has no URLs, so relative links and images are left as written.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkBase {
    pub blob: String,
    pub tree: String,
    pub raw: String,
    pub directory: String,
}

/// What the Markdown renderer appends to every link's `href`.
const NEW_TAB: &str = " target=\"_blank\" rel=\"noopener noreferrer\"";

impl LinkBase {
    /// The target of a relative `url`, or `None` when it is not relative.
    fn resolve(&self, url: &str, image: bool) -> Option<String> {
        let end = url.find(['?', '#']).unwrap_or(url.len());
        let (path, suffix) = url.split_at(end);
        if path.is_empty() {
            // Parsing keeps a discussion's `#comment-N` as the page anchor. A repository file
            // has no such anchor, so there the same link names the heading with that id.
            return suffix
                .strip_prefix('#')
                .filter(|_| !image)
                .map(|fragment| format!("#{}", heading_target(fragment)));
        }
        // Without a base a relative address has nothing to resolve against.
        if self.blob.is_empty() {
            return None;
        }
        let before_slash = path.split('/').next().unwrap_or("");
        if path.starts_with('/') || before_slash.contains(':') {
            return None;
        }
        let mut parts: Vec<&str> = self
            .directory
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        // The repository root is a directory, which has no blob page.
        let root = if image {
            &self.raw
        } else if path.ends_with('/') || parts.is_empty() {
            &self.tree
        } else {
            &self.blob
        };
        let joined = parts.join("/");
        // Headings in a Markdown file carry the `user-content-` prefix, so its fragments do too.
        let markdown_file = crate::webui::pages::repos::ref_path::is_markdown_path(path);
        let suffix = match suffix.split_once('#') {
            // The code view keeps the file's line anchors as they are.
            Some((query, fragment))
                if markdown_file
                    && !fragment.is_empty()
                    && !query
                        .trim_start_matches('?')
                        .split('&')
                        .any(|pair| pair == "view=code") =>
            {
                format!("{query}#{}", heading_target(fragment))
            }
            _ => suffix.to_owned(),
        };
        Some(if joined.is_empty() {
            format!("{root}{suffix}")
        } else {
            format!("{root}/{joined}{suffix}")
        })
    }

    /// A `srcset` with the address of each candidate resolved. A candidate keeps its descriptor.
    fn resolve_srcset(&self, srcset: &str) -> String {
        let Some(candidates) = super::markdown_html::srcset_candidates(srcset) else {
            return String::new();
        };
        let mut out = String::new();
        for (index, (url, descriptor)) in candidates.into_iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&self.resolve(url, true).unwrap_or_else(|| url.to_owned()));
            if !descriptor.is_empty() {
                out.push(' ');
                out.push_str(descriptor);
            }
        }
        out
    }

    /// Rewrites the relative links and images of rendered Markdown. Only the renderer's own
    /// `<a href="`, `<img src="` and `<source srcset="` openings count: source text is escaped.
    fn rewrite(&self, html: &str) -> Option<String> {
        const OPENINGS: [&str; 3] = ["<a href=\"", "<img src=\"", "<source srcset=\""];
        let mut out = String::with_capacity(html.len().min(MAX_MARKDOWN_BYTES));
        let mut pos = 0;
        // Where each opening is found next. Searching the rest of the page again for every
        // link, with no image to find, would be quadratic.
        let mut found: [Option<Option<usize>>; 3] = [None, None, None];
        loop {
            let mut next: Option<(usize, usize)> = None;
            for (kind, opening) in OPENINGS.iter().enumerate() {
                let current = match found[kind] {
                    Some(None) => true,
                    Some(Some(at)) => at >= pos,
                    None => false,
                };
                if !current {
                    found[kind] = Some(html[pos..].find(opening).map(|at| at + pos));
                }
                if let Some(Some(at)) = found[kind] {
                    if next.is_none_or(|(best, _)| at < best) {
                        next = Some((at, kind));
                    }
                }
            }
            let Some((at, kind)) = next else { break };
            let image = kind != 0;
            // The third opening is a `srcset`, which has an address for each candidate.
            let srcset = kind == 2;
            let start = at + OPENINGS[kind].len();
            append_html(&mut out, &html[pos..start])?;
            pos = start;
            let end = html[pos..].find('"').map_or(html.len(), |end| pos + end);
            let serialized = &html[pos..end];
            let value = decode_references(serialized).0;
            let mut in_page = value.starts_with('#');
            if srcset {
                append_html(&mut out, &escape_html(&self.resolve_srcset(&value)))?;
            } else {
                match self.resolve(&value, image) {
                    Some(target) => {
                        append_html(&mut out, &escape_html(&target))?;
                        in_page = true;
                    }
                    None => append_html(&mut out, serialized)?,
                }
            }
            pos = end;
            // Links within the repository open in place, not in a new tab.
            if in_page && !image {
                if let Some(after) = html[pos..].strip_prefix(&format!("\"{NEW_TAB}")) {
                    append_html(&mut out, "\"")?;
                    pos = html.len() - after.len();
                }
            }
        }
        append_html(&mut out, &html[pos..])?;
        Some(out)
    }
}

/// The class of one Write or Preview tab. The tabs are buttons, and the one showing is marked
/// active. Collaboration comments and repository files share these styles.
pub fn markdown_tab_class(active: bool) -> &'static str {
    if active {
        "collaboration-editor-tab collaboration-editor-tab-button collaboration-editor-tab-active"
    } else {
        "collaboration-editor-tab collaboration-editor-tab-button"
    }
}

/// Markdown from a file in a repository, with relative links and images resolved the way
/// the code host's file browser would.
#[component]
pub fn RepoMarkdown(
    content: String,
    base: LinkBase,
    #[props(default)] references: References,
) -> Element {
    let html = document_html(&content, &references, &base);

    rsx! {
        div { class: "markdown-body leading-relaxed text-foreground", dangerous_inner_html: "{html}" }
    }
}

/// Render once; callers with an outline read it from this same HTML.
pub fn document_html(content: &str, references: &References, base: &LinkBase) -> String {
    base.rewrite(&parse_document(content, references))
        .unwrap_or_else(|| plain_text_preview(content))
}

/// One heading of a Markdown file, for the outline menu.
#[derive(Clone, Debug, PartialEq)]
pub struct OutlineEntry {
    pub level: usize,
    pub text: String,
    pub id: String,
}

/// The headings of a Markdown file in order. They are read back from the HTML the page renders,
/// so each id is the one its section has.
pub fn outline_of_html(html: &str) -> Vec<OutlineEntry> {
    let mut entries = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<h") {
        let after = &rest[start + 2..];
        let Some(level) = after
            .chars()
            .next()
            .and_then(|digit| digit.to_digit(10))
            .filter(|level| (1..=6).contains(level))
        else {
            rest = after;
            continue;
        };
        let Some(open_end) = after.find('>') else {
            break;
        };
        let body = &after[open_end + 1..];
        let close = format!("</h{level}>");
        let Some(end) = body.find(close.as_str()) else {
            break;
        };
        let id = after[..open_end]
            .split_once(" id=\"")
            .and_then(|(_, value)| value.split_once('"'))
            .map(|(id, _)| id.to_owned());
        if let Some(id) = id {
            entries.push(OutlineEntry {
                level: level as usize,
                text: heading_text(&body[..end]),
                id,
            });
        }
        rest = &body[end + close.len()..];
    }
    entries
}

/// The text of a heading's HTML: its tags removed and its escaped characters restored.
fn heading_text(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(character),
            _ => {}
        }
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_owned()
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
