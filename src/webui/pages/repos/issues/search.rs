//! The search text of an issue or pull request list. The box holds the filters as qualifiers, such
//! as `state:closed` and `label:"two words"`, and the words left over are the search. The text is
//! read into the filters and written from them here, so the box and the filter menus agree.
use super::{label_hex, Kind, ListFilters};
use std::collections::HashMap;
use std::ops::Range;

/// The filters a search text names, and the words that are not qualifiers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Search {
    /// `open`, `closed` or `all`.
    pub state: Option<String>,
    pub author: Option<String>,
    /// A username, or `none` for no assignee.
    pub assignee: Option<String>,
    pub milestone: Option<MilestoneFilter>,
    pub sort: Option<String>,
    /// The label names, in the order the text gives them.
    pub labels: Vec<String>,
    /// The words that are not qualifiers, joined by single spaces.
    pub text: String,
}

/// The milestone a search names. `no:milestone` is its own choice, so a milestone titled `none` is
/// still found by its title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MilestoneFilter {
    Title(String),
    NoMilestone,
}

impl Search {
    /// Writes the qualifiers over the request's parameters, so the list reads the text in place of
    /// them. The text's words become the search.
    pub fn apply_to(&self, query: &mut HashMap<String, String>) {
        let values = [
            ("state", &self.state),
            ("author", &self.author),
            ("assignee", &self.assignee),
            ("sort", &self.sort),
        ];
        for (name, value) in values {
            if let Some(value) = value {
                query.insert(name.to_owned(), value.clone());
            }
        }
        // The list names no milestone as `none`, and a title has already been read as a number.
        match &self.milestone {
            Some(MilestoneFilter::Title(title)) => {
                query.insert("milestone".to_owned(), title.clone());
            }
            Some(MilestoneFilter::NoMilestone) => {
                query.insert("milestone".to_owned(), "none".to_owned());
            }
            None => {}
        }
        query.insert("q".to_owned(), self.text.clone());
    }
}

/// One qualifier word that a list offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Qualifier {
    /// A kind word such as `is:pr`. The list is already one kind, so the word changes nothing.
    Kind,
    State(String),
    Author(String),
    Assignee(String),
    Milestone(String),
    NoMilestone,
    Label(String),
    Sort(String),
}

/// Reads a search text for a list of `kind`. A word that is not a qualifier the list offers stays
/// in the text, so a search for it finds the words as they were typed.
pub fn parse_search(text: &str) -> Search {
    let mut search = Search::default();
    let mut words = Vec::new();
    for span in word_spans(text) {
        let word = &text[span];
        match qualifier(word) {
            Some(Qualifier::Kind) => {}
            Some(Qualifier::State(value)) => search.state = Some(value),
            Some(Qualifier::Author(value)) => search.author = Some(value),
            Some(Qualifier::Assignee(value)) => search.assignee = Some(value),
            Some(Qualifier::Milestone(value)) => {
                search.milestone = Some(MilestoneFilter::Title(value));
            }
            Some(Qualifier::NoMilestone) => {
                search.milestone = Some(MilestoneFilter::NoMilestone);
            }
            Some(Qualifier::Label(value)) => search.labels.push(value),
            Some(Qualifier::Sort(value)) => search.sort = Some(value),
            None => words.push(word),
        }
    }
    search.text = words.join(" ");
    search
}

/// The qualifier a word names for a list of `kind`, or None when the word is text.
pub(super) fn qualifier(word: &str) -> Option<Qualifier> {
    let (key, raw) = word.split_once(':')?;
    let value = unquote(raw);
    if value.is_empty() {
        return None;
    }
    let key = key.to_ascii_lowercase();
    if key == "is" && matches!(value.as_str(), "issue" | "pr") {
        return Some(Qualifier::Kind);
    }
    let state = match key.as_str() {
        "is" => matches!(value.as_str(), "open" | "closed"),
        "state" => matches!(value.as_str(), "open" | "closed" | "all"),
        _ => false,
    };
    if state {
        return Some(Qualifier::State(value));
    }
    match key.as_str() {
        "author" => Some(Qualifier::Author(value)),
        "assignee" => Some(Qualifier::Assignee(value)),
        "milestone" => Some(Qualifier::Milestone(value)),
        "label" => Some(Qualifier::Label(value)),
        "sort"
            if matches!(
                value.as_str(),
                "newest"
                    | "oldest"
                    | "recently-updated"
                    | "least-recently-updated"
                    | "most-commented"
                    | "least-commented"
            ) =>
        {
            Some(Qualifier::Sort(value))
        }
        "no" if value == "assignee" => Some(Qualifier::Assignee("none".to_owned())),
        "no" if value == "milestone" => Some(Qualifier::NoMilestone),
        _ => None,
    }
}

/// The value of a qualifier word without its surrounding double quotes. Inside the quotes a
/// backslash escapes the next character, so a quote or a backslash can be part of a value.
fn unquote(raw: &str) -> String {
    match raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        Some(inner) => unescape(inner),
        None => raw.to_owned(),
    }
}

/// `inner` with each escaped character read as itself. A backslash with nothing after it stays.
fn unescape(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
        } else {
            out.push(chars.next().unwrap_or('\\'));
        }
    }
    out
}

/// The byte ranges of the words in a search text. A word ends at a space outside double quotes, so
/// `label:"two words"` is one word. Inside quotes a backslash escapes the next character, so an
/// escaped quote does not end the quotes.
pub(super) fn word_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = None;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if ch.is_whitespace() && !quoted {
            if let Some(begin) = start.take() {
                spans.push(begin..index);
            }
            continue;
        }
        if escaped {
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        }
        start.get_or_insert(index);
    }
    if let Some(begin) = start {
        spans.push(begin..text.len());
    }
    spans
}

/// The search text that names `filters` for a list in `state`, followed by the search words. The
/// box shows this text, and reading it back gives the same filters.
pub fn search_text(kind: Kind, state: &str, filters: &ListFilters, text: &str) -> String {
    let mut parts = vec![match kind {
        Kind::Issue => "is:issue".to_owned(),
        Kind::Pull => "is:pr".to_owned(),
    }];
    let state = if state.is_empty() { "open" } else { state };
    parts.push(format!("state:{state}"));
    if !filters.author.is_empty() {
        parts.push(qualifier_text("author", &filters.author));
    }
    match filters.assignee.as_str() {
        "" => {}
        "none" => parts.push("no:assignee".to_owned()),
        name => parts.push(qualifier_text("assignee", name)),
    }
    parts.extend(
        filters
            .selected_labels
            .iter()
            .map(|name| qualifier_text("label", name)),
    );
    match filters.milestone.as_str() {
        "" => {}
        "none" => parts.push("no:milestone".to_owned()),
        number => {
            let title = filters
                .milestones
                .iter()
                .find(|milestone| milestone.number.to_string() == number)
                .map_or(number, |milestone| milestone.title.as_str());
            parts.push(qualifier_text("milestone", title));
        }
    }
    if !filters.sort.is_empty() && filters.sort != "newest" {
        parts.push(qualifier_text("sort", &filters.sort));
    }
    if !text.is_empty() {
        parts.push(text.to_owned());
    }
    parts.join(" ")
}

/// `key:value`, with the value in double quotes when it has a space or a quote. Inside the quotes a
/// backslash and a quote are escaped, so `parse_search` reads the value back as it was.
pub(super) fn qualifier_text(key: &str, value: &str) -> String {
    if value.chars().any(|ch| ch.is_whitespace() || ch == '"') {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{key}:\"{escaped}\"")
    } else {
        format!("{key}:{value}")
    }
}

/// The qualifier names the box offers: the key, the title shown for it, and a hint.
const NAMES: [(&str, &str, &str); 7] = [
    ("state", "State", "Open, closed or all"),
    ("author", "Author", "Opened by a person"),
    ("label", "Label", "Carries a label"),
    ("assignee", "Assignee", "Assigned to a person"),
    ("milestone", "Milestone", "In a milestone"),
    ("sort", "Sort", "Order of the list"),
    ("is", "Is", "Open or closed"),
];

/// The sort values the box offers, with their titles.
const SORTS: [(&str, &str); 6] = [
    ("newest", "Newest"),
    ("oldest", "Oldest"),
    ("most-commented", "Most commented"),
    ("least-commented", "Least commented"),
    ("recently-updated", "Recently updated"),
    ("least-recently-updated", "Least recently updated"),
];

/// The most people the author and assignee suggestions show.
const MAX_PEOPLE: usize = 50;

/// One row of the suggestion list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Suggestion {
    /// The text that replaces the word being typed.
    pub insert: String,
    pub text: String,
    /// A muted hint after the text. Empty when there is none.
    pub hint: String,
    /// A label colour as `#rrggbb`, shown as a dot.
    pub colour: Option<String>,
    /// Whether the row names a qualifier, which shows the qualifier icon.
    pub name: bool,
}

/// The suggestions for the word being typed. A word without a colon names a qualifier; a word with
/// one suggests values for that qualifier.
pub(super) fn suggestions(word: &str, filters: &ListFilters) -> Vec<Suggestion> {
    match word.split_once(':') {
        None => {
            let typed = word.to_ascii_lowercase();
            NAMES
                .iter()
                .filter(|(key, _, _)| key.starts_with(typed.as_str()))
                .map(|(key, title, hint)| Suggestion {
                    insert: format!("{key}:"),
                    text: (*title).to_owned(),
                    hint: (*hint).to_owned(),
                    colour: None,
                    name: true,
                })
                .collect()
        }
        Some((key, raw)) => value_suggestions(&key.to_ascii_lowercase(), raw, filters),
    }
}

/// The values a qualifier suggests, for the text typed after its colon.
fn value_suggestions(key: &str, raw: &str, filters: &ListFilters) -> Vec<Suggestion> {
    let typed = raw.trim_matches('"').to_lowercase();
    match key {
        "state" | "is" => {
            let mut states = vec![("open", "Open"), ("closed", "Closed")];
            if key == "state" {
                states.push(("all", "All"));
            }
            states
                .into_iter()
                .filter(|(value, _)| contains(value, &typed))
                .map(|(value, text)| plain(key, value, text))
                .collect()
        }
        "author" | "assignee" => people(filters)
            .into_iter()
            .filter(|name| contains(name, &typed))
            .take(MAX_PEOPLE)
            .map(|name| plain(key, &name, &name))
            .collect(),
        "label" => filters
            .labels
            .iter()
            .filter(|label| contains(&label.name, &typed))
            .map(|label| Suggestion {
                insert: qualifier_text(key, &label.name),
                text: label.name.clone(),
                hint: String::new(),
                colour: label_hex(&label.color),
                name: false,
            })
            .collect(),
        "milestone" => filters
            .milestones
            .iter()
            .filter(|milestone| contains(&milestone.title, &typed))
            .map(|milestone| Suggestion {
                insert: qualifier_text(key, &milestone.title),
                text: milestone.title.clone(),
                hint: if milestone.state == "closed" {
                    "Closed"
                } else {
                    "Open"
                }
                .to_owned(),
                colour: None,
                name: false,
            })
            .collect(),
        "sort" => SORTS
            .iter()
            .filter(|(_, title)| contains(title, &typed))
            .map(|(value, title)| plain(key, value, title))
            .collect(),
        _ => Vec::new(),
    }
}

/// A suggestion that inserts `key:value` and shows `text`.
fn plain(key: &str, value: &str, text: &str) -> Suggestion {
    Suggestion {
        insert: qualifier_text(key, value),
        text: text.to_owned(),
        hint: String::new(),
        colour: None,
        name: false,
    }
}

/// Whether `text` contains `typed`, ignoring case. An empty `typed` contains everything.
fn contains(text: &str, typed: &str) -> bool {
    text.to_lowercase().contains(typed)
}

/// The people the author and assignee suggestions offer: each name in the author or the assignee
/// list, once.
fn people(filters: &ListFilters) -> Vec<String> {
    let mut names: Vec<String> = filters
        .authors
        .iter()
        .chain(&filters.assignees)
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The byte range of the word the caret is in or touches, if any. The caret is a byte index.
pub(super) fn word_at(text: &str, caret: usize) -> Option<Range<usize>> {
    word_spans(text)
        .into_iter()
        .find(|span| span.start <= caret && caret <= span.end)
}

/// Replaces the word at `span` with `insert`, and returns the new text with the caret's byte index.
/// A name ends in a colon, so the caret stays against it for the value list. Anything else is
/// followed by a space, unless the text already has a space there.
pub(super) fn insert_word(text: &str, span: Range<usize>, insert: &str) -> (String, usize) {
    let rest = &text[span.end..];
    let space = if insert.ends_with(':') || rest.starts_with(char::is_whitespace) {
        ""
    } else {
        " "
    };
    let caret = span.start + insert.len() + space.len();
    let next = format!("{}{insert}{space}{rest}", &text[..span.start]);
    (next, caret)
}

/// The byte index of a UTF-16 offset. The browser counts a text box's caret in UTF-16 units.
pub(super) fn byte_index(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units >= utf16 {
            return index;
        }
        units += ch.len_utf16();
    }
    text.len()
}

/// The UTF-16 offset of a byte index.
pub(super) fn utf16_index(text: &str, byte: usize) -> usize {
    text[..byte].encode_utf16().count()
}

/// The text cut into pieces for the box's highlight layer. Each piece says whether it is a
/// qualifier word.
pub(super) fn highlight_pieces(text: &str) -> Vec<(bool, &str)> {
    let mut pieces = Vec::new();
    let mut at = 0;
    for span in word_spans(text) {
        if span.start > at {
            pieces.push((false, &text[at..span.start]));
        }
        let word = &text[span.clone()];
        pieces.push((qualifier(word).is_some(), word));
        at = span.end;
    }
    if at < text.len() {
        pieces.push((false, &text[at..]));
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webui::pages::repos::issues::{Label, Milestone};

    fn milestone(number: i32, title: &str, state: &str) -> Milestone {
        Milestone {
            number,
            title: title.into(),
            description: None,
            due_date: None,
            state: state.into(),
            open_items: 0,
            closed_items: 0,
        }
    }

    fn label(name: &str, color: &str) -> Label {
        Label {
            id: 1,
            name: name.into(),
            color: color.into(),
            description: String::new(),
        }
    }

    fn keys(rows: &[Suggestion]) -> Vec<&str> {
        rows.iter().map(|row| row.insert.as_str()).collect()
    }

    #[test]
    fn the_default_box_names_the_kind_and_the_open_state() {
        let filters = ListFilters::default();
        assert_eq!(
            search_text(Kind::Issue, "open", &filters, ""),
            "is:issue state:open"
        );
        assert_eq!(
            search_text(Kind::Pull, "open", &filters, ""),
            "is:pr state:open"
        );
    }

    #[test]
    fn the_box_names_each_filter_and_then_the_words() {
        let filters = ListFilters {
            author: "alice".into(),
            assignee: "none".into(),
            selected_labels: vec!["bug".into(), "two words".into()],
            milestone: "3".into(),
            milestones: vec![milestone(3, "Release 1", "open")],
            sort: "oldest".into(),
            ..Default::default()
        };
        assert_eq!(
            search_text(Kind::Issue, "closed", &filters, "login bug"),
            "is:issue state:closed author:alice no:assignee label:bug label:\"two words\" milestone:\"Release 1\" sort:oldest login bug"
        );
    }

    #[test]
    fn reading_the_box_gives_back_the_same_filters() {
        let text = "is:issue state:closed author:alice label:bug label:\"two words\" milestone:\"Release 1\" sort:oldest login bug";
        assert_eq!(
            parse_search(text),
            Search {
                state: Some("closed".into()),
                author: Some("alice".into()),
                assignee: None,
                milestone: Some(MilestoneFilter::Title("Release 1".into())),
                sort: Some("oldest".into()),
                labels: vec!["bug".into(), "two words".into()],
                text: "login bug".into(),
            }
        );
    }

    #[test]
    fn none_values_and_kind_words_are_qualifiers() {
        let search = parse_search("is:pr no:assignee no:milestone");
        assert_eq!(search.assignee.as_deref(), Some("none"));
        assert_eq!(search.milestone, Some(MilestoneFilter::NoMilestone));
        assert_eq!(search.text, "");
        // A kind word names the list it is already on, and changes nothing.
        assert_eq!(parse_search("is:pr").text, "");
        assert_eq!(parse_search("is:issue").text, "");
    }

    #[test]
    fn other_words_stay_in_the_text() {
        let search = parse_search("state:merged no:label foo:bar");
        assert_eq!(search.state, None);
        assert_eq!(search.text, "state:merged no:label foo:bar");
    }

    #[test]
    fn values_the_list_does_not_offer_stay_in_the_text() {
        let search = parse_search("state:bogus sort:random author: title:x");
        assert_eq!(search.state, None);
        assert_eq!(search.sort, None);
        assert_eq!(search.author, None);
        assert_eq!(search.text, "state:bogus sort:random author: title:x");
    }

    #[test]
    fn quoted_phrases_keep_their_spaces_and_quotes_survive_a_round_trip() {
        let search = parse_search("   \"login  bug\"    crash ");
        assert_eq!(search.text, "\"login  bug\" crash");
        let quoted = ListFilters {
            selected_labels: vec!["a\"b".into()],
            ..Default::default()
        };
        let text = search_text(Kind::Issue, "open", &quoted, "");
        assert_eq!(parse_search(&text).labels, ["a\"b"]);
    }

    #[test]
    fn a_quote_or_backslash_in_a_name_survives_the_qualifiers_after_it() {
        let filters = ListFilters {
            author: "x\"y".into(),
            selected_labels: vec!["a\"b".into(), "c\\d e".into()],
            ..Default::default()
        };
        let text = search_text(Kind::Issue, "closed", &filters, "crash");
        assert_eq!(
            text,
            "is:issue state:closed author:\"x\\\"y\" label:\"a\\\"b\" label:\"c\\\\d e\" crash"
        );
        let search = parse_search(&text);
        assert_eq!(search.author.as_deref(), Some("x\"y"));
        assert_eq!(search.labels, ["a\"b", "c\\d e"]);
        assert_eq!(search.state.as_deref(), Some("closed"));
        assert_eq!(search.text, "crash");
    }

    #[test]
    fn a_milestone_titled_none_is_a_title_and_no_milestone_is_its_own_choice() {
        assert_eq!(
            parse_search("milestone:none").milestone,
            Some(MilestoneFilter::Title("none".into()))
        );
        assert_eq!(
            parse_search("no:milestone").milestone,
            Some(MilestoneFilter::NoMilestone)
        );
        // The last milestone qualifier in the text is the one that applies.
        assert_eq!(
            parse_search("milestone:none no:milestone").milestone,
            Some(MilestoneFilter::NoMilestone)
        );
    }

    #[test]
    fn a_word_without_a_colon_suggests_the_names_it_starts() {
        let rows = suggestions("a", &ListFilters::default());
        assert_eq!(keys(&rows), ["author:", "assignee:"]);
        assert!(rows.iter().all(|row| row.name));
    }

    #[test]
    fn a_label_suggestion_carries_its_colour_and_quotes_a_spaced_name() {
        let filters = ListFilters {
            labels: vec![label("bug", "d73a4a"), label("two words", "zz")],
            ..Default::default()
        };
        let rows = suggestions("label:", &filters);
        assert_eq!(keys(&rows), ["label:bug", "label:\"two words\""]);
        assert_eq!(rows[0].colour.as_deref(), Some("#d73a4a"));
        assert_eq!(rows[1].colour, None);
        let found = suggestions("label:two", &filters);
        assert_eq!(keys(&found), ["label:\"two words\""]);
    }

    #[test]
    fn people_are_offered_once_and_capped() {
        let filters = ListFilters {
            authors: vec!["alice".into(), "bob".into()],
            assignees: vec!["bob".into(), "carol".into()],
            ..Default::default()
        };
        let rows = suggestions("author:", &filters);
        assert_eq!(keys(&rows), ["author:alice", "author:bob", "author:carol"]);
        let many = ListFilters {
            authors: (0..80).map(|n| format!("user{n:02}")).collect(),
            ..Default::default()
        };
        assert_eq!(suggestions("assignee:", &many).len(), MAX_PEOPLE);
        assert_eq!(suggestions("assignee:user79", &many).len(), 1);
    }

    #[test]
    fn state_values_are_offered_for_state_and_is() {
        let filters = ListFilters::default();
        assert_eq!(
            keys(&suggestions("state:", &filters)),
            ["state:open", "state:closed", "state:all"]
        );
        assert_eq!(keys(&suggestions("is:op", &filters)), ["is:open"]);
    }

    #[test]
    fn milestone_suggestions_name_the_title_and_its_state() {
        let filters = ListFilters {
            milestones: vec![milestone(3, "Release 1", "closed")],
            ..Default::default()
        };
        let rows = suggestions("milestone:", &filters);
        assert_eq!(keys(&rows), ["milestone:\"Release 1\""]);
        assert_eq!(rows[0].hint, "Closed");
    }

    #[test]
    fn a_suggestion_replaces_the_word_at_the_caret() {
        // "is:issue sta login": the word "sta" is bytes 9..12.
        let text = "is:issue sta login";
        let span = word_at(text, 12).expect("the caret touches the word");
        assert_eq!(span, 9..12);
        let (next, caret) = insert_word(text, span, "state:");
        assert_eq!(next, "is:issue state: login");
        assert_eq!(caret, 15);
    }

    #[test]
    fn a_value_suggestion_ends_with_a_space_unless_one_follows() {
        let text = "is:issue state:op";
        let span = word_at(text, text.len()).expect("the caret is at the end");
        let (next, caret) = insert_word(text, span, "state:open");
        assert_eq!(next, "is:issue state:open ");
        assert_eq!(caret, next.len());
        let (kept, _) = insert_word("is:issue state:op later", 9..17, "state:open");
        assert_eq!(kept, "is:issue state:open later");
    }

    #[test]
    fn no_word_sits_at_a_caret_after_a_space() {
        assert_eq!(word_at("is:issue ", 9), None);
        assert_eq!(word_at("is:issue", 8), Some(0..8));
    }

    #[test]
    fn carets_convert_between_utf16_units_and_bytes() {
        // "a" is one unit, the emoji is two units and four bytes.
        assert_eq!(byte_index("a\u{1F600}b", 3), 5);
        assert_eq!(utf16_index("a\u{1F600}b", 5), 3);
        assert_eq!(byte_index("abc", 9), 3);
    }

    #[test]
    fn highlight_pieces_mark_only_the_qualifier_words() {
        let pieces = highlight_pieces("is:issue  login state:open");
        assert_eq!(
            pieces,
            [
                (true, "is:issue"),
                (false, "  "),
                (false, "login"),
                (false, " "),
                (true, "state:open"),
            ]
        );
    }
}
