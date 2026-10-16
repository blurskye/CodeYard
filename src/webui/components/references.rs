//! GitHub-style references in rendered Markdown: `#123`, `@user`, a bare commit SHA,
//! `owner/repo#123` and `owner/repo@sha`. This module classifies the text. The handler decides
//! which targets exist and which the viewer can read, and passes that answer in `References`,
//! so the server and the browser render the same links.

use super::markdown::collect_candidates;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ops::Range;

/// Distinct usernames a page resolves. Later names stay plain text.
pub const MAX_USERS: usize = 50;
/// Distinct other repositories a page resolves. Later ones stay plain text.
pub const MAX_REPOSITORIES: usize = 10;
/// Longest owner or repository name. Usernames are bounded in bytes by the account rules.
const MAX_NAME_BYTES: usize = 1024;

/// What the handler resolved for one page. The vectors are sorted, so every rendering of the
/// same page model produces the same links.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct References {
    /// The repository the page belongs to. `#N` and commit SHAs link into it.
    pub owner: String,
    pub repo: String,
    /// The highest issue or pull request number. Numbers are never reused and items are never
    /// deleted, so `#N` is an item exactly when `1 <= N <= last_number`.
    pub last_number: i32,
    /// Existing usernames, spelled as stored.
    pub users: Vec<String>,
    /// Other repositories the viewer can read. A repository that is missing or unreadable is
    /// absent, so its references stay plain text.
    pub repositories: Vec<Repository>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Repository {
    pub owner: String,
    pub name: String,
    pub last_number: i32,
}

/// A reference as the text spells it.
#[derive(Debug, PartialEq)]
pub enum Token<'a> {
    Number(i64),
    User(&'a str),
    Commit(&'a str),
    RepoNumber {
        owner: &'a str,
        repo: &'a str,
        number: i64,
    },
    RepoCommit {
        owner: &'a str,
        repo: &'a str,
        sha: &'a str,
    },
}

/// A linked reference, before escaping.
pub struct Link {
    pub href: String,
    pub class: &'static str,
    /// The full commit SHA, shown on hover while the link text is shortened.
    pub title: Option<String>,
    pub text: String,
}

const REFERENCE: &str = "markdown-reference";
const COMMIT: &str = "markdown-reference markdown-commit";

impl References {
    /// The link `token` makes on this page, or `None` when it stays plain text. The default
    /// value names no repository and links nothing.
    pub fn link(&self, token: &Token<'_>) -> Option<Link> {
        if self.repo.is_empty() {
            return None;
        }
        match *token {
            Token::Number(number) => {
                self.issue_link(&self.owner, &self.repo, number, || format!("#{number}"))
            }
            Token::RepoNumber {
                owner,
                repo,
                number,
            } => self.issue_link(owner, repo, number, || format!("{owner}/{repo}#{number}")),
            Token::User(name) => self
                .users
                .binary_search_by(|user| user.as_str().cmp(name))
                .ok()
                .map(|_| Link {
                    href: format!("/{name}"),
                    class: REFERENCE,
                    title: None,
                    text: format!("@{name}"),
                }),
            Token::Commit(sha) => Some(commit_link(
                &self.owner,
                &self.repo,
                sha,
                sha[..7].to_owned(),
            )),
            Token::RepoCommit { owner, repo, sha } => {
                self.repository_last_number(owner, repo)?;
                Some(commit_link(
                    owner,
                    repo,
                    sha,
                    format!("{owner}/{repo}@{}", &sha[..7]),
                ))
            }
        }
    }

    /// The link to issue or pull request `number`, when the page may link it. Its text is made only
    /// for a link, so a token that stays plain allocates nothing.
    fn issue_link(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        text: impl FnOnce() -> String,
    ) -> Option<Link> {
        let last = self.repository_last_number(owner, repo)?;
        (number <= last).then(|| Link {
            href: format!("/{owner}/{repo}/issues/{number}"),
            class: REFERENCE,
            title: None,
            text: text(),
        })
    }

    /// The last number of `owner/repo`, when the page may link into that repository.
    fn repository_last_number(&self, owner: &str, repo: &str) -> Option<i64> {
        if owner == self.owner && repo == self.repo {
            return Some(i64::from(self.last_number));
        }
        self.repositories
            .iter()
            .find(|other| other.owner == owner && other.name == repo)
            .map(|other| i64::from(other.last_number))
    }
}

fn commit_link(owner: &str, repo: &str, sha: &str, text: String) -> Link {
    Link {
        href: format!("/{owner}/{repo}/commit/{sha}"),
        class: COMMIT,
        title: Some(sha.to_owned()),
        text,
    }
}

/// Calls `visit` with each reference token of `text[range]`, in order, with its byte range. A token
/// is a whole word, so `foo#12` and `#12#13` are not references. Bytes in `opaque` are escaped
/// characters and decoded entities, which the source does not spell as written. They separate
/// words no differently from other bytes, so a word that holds one yields no token: `#1&#50;3`
/// renders `#123`, and must not link `#1`.
pub(super) fn scan_tokens<'a>(
    text: &'a str,
    range: Range<usize>,
    opaque: &[Range<usize>],
    mut visit: impl FnMut(Range<usize>, Token<'a>),
) {
    let mut word: Option<usize> = None;
    let mut word_is_opaque = false;
    // Both the characters and the opaque ranges are in order, so one cursor walks each once.
    let mut cursor = opaque.partition_point(|span| span.end <= range.start);
    for (offset, character) in text[range.clone()].char_indices() {
        let at = range.start + offset;
        while opaque.get(cursor).is_some_and(|span| span.end <= at) {
            cursor += 1;
        }
        let opaque_here = opaque.get(cursor).is_some_and(|span| span.start <= at);
        if is_word(character) {
            if word.is_none() {
                word = Some(at);
                word_is_opaque = false;
            }
            word_is_opaque |= opaque_here;
        } else if let Some(start) = word.take() {
            if !word_is_opaque {
                push_token(text, start..at, &mut visit);
            }
        }
    }
    if let Some(start) = word {
        if !word_is_opaque {
            push_token(text, start..range.end, &mut visit);
        }
    }
}

/// Characters a reference word is made of: letters and digits, and the punctuation that names
/// contain. Any other character ends a word.
fn is_word(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '-' | '_' | '.' | '/' | '#' | '@')
}

/// Classifies the whole word at `word`. Trailing dots end a sentence, not a reference, so
/// `#12.` links.
fn push_token<'a>(
    text: &'a str,
    word: Range<usize>,
    visit: &mut impl FnMut(Range<usize>, Token<'a>),
) {
    let spelled = &text[word.clone()];
    let trimmed = spelled.trim_end_matches('.');
    if let Some(token) = classify(trimmed) {
        visit(word.start..word.start + trimmed.len(), token);
    }
}

/// The reference a whole word spells, if it spells one. Each form is matched from its start to
/// its end, with no backtracking.
fn classify(word: &str) -> Option<Token<'_>> {
    if let Some(digits) = word.strip_prefix('#') {
        return number(digits).map(Token::Number);
    }
    if let Some(name) = word.strip_prefix('@') {
        return owner_name(name).then_some(Token::User(name));
    }
    let Some((owner, rest)) = word.split_once('/') else {
        return commit(word).map(Token::Commit);
    };
    let (repo, tail) = rest.split_at(rest.find(['#', '@'])?);
    if !owner_name(owner) || !repo_name(repo) {
        return None;
    }
    match tail.strip_prefix('#') {
        Some(digits) => number(digits).map(|number| Token::RepoNumber {
            owner,
            repo,
            number,
        }),
        None => commit(tail.strip_prefix('@')?).map(|sha| Token::RepoCommit { owner, repo, sha }),
    }
}

/// The `N` of `#N`: one to ten digits with no leading zero.
fn number(digits: &str) -> Option<i64> {
    let valid = (1..=10).contains(&digits.len())
        && !digits.starts_with('0')
        && digits.bytes().all(|byte| byte.is_ascii_digit());
    valid.then(|| digits.parse::<i64>().ok()).flatten()
}

/// A bare commit SHA: seven to forty hex digits from 0 to 9 and a to f, with at least one digit
/// and one letter. Numbers and plain words stay plain, as do most hashes of other things.
fn commit(value: &str) -> Option<&str> {
    let hex = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
    let valid = (7..=40).contains(&value.len())
        && value.bytes().all(hex)
        && value.bytes().any(|byte| byte.is_ascii_digit())
        && value.bytes().any(|byte| (b'a'..=b'f').contains(&byte));
    valid.then_some(value)
}

/// Usernames and organization names: letters, digits, `-` and `_`.
fn owner_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// Repository names: letters, digits, `-`, `_` and `.`.
fn repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// What a page's Markdown could link. The handler resolves only these names.
#[derive(Debug, Default, PartialEq)]
pub struct Candidates {
    pub users: Vec<String>,
    /// Distinct `(owner, repo)` pairs, as written, other than the page's own repository.
    pub repositories: Vec<(String, String)>,
    /// The page's own repository. Its references need no lookup, so they take no place in
    /// `repositories`, which is capped.
    current: (String, String),
    /// The most distinct names `users` keeps. A page keeps `MAX_USERS`; a mention scan keeps all.
    user_limit: usize,
    /// The names in `users`, so a repeated name is found without scanning the list.
    seen_users: HashSet<String>,
}

/// The candidates of `texts` for a page of the repository `current`, given as owner and name. The
/// texts are read in the mode the page renders them with: as a document when `document` is set (a
/// repository file), otherwise as a comment. The renderer's own emitter reads them, so text it
/// leaves unlinked (code blocks, links, images and raw `a`, `code` and `pre` elements) takes no part
/// in the budgets. Every reference the renderer can link is among the candidates.
pub fn candidates(texts: &[&str], document: bool, current: (&str, &str)) -> Candidates {
    walk(texts, document, current, MAX_USERS)
}

/// The usernames `text` mentions as the renderer links them: distinct and in first-seen order,
/// with no page cap. Code, links, images, escaped text and raw code elements mention nobody.
pub fn mentioned_usernames(text: &str) -> Vec<String> {
    walk(&[text], false, ("", ""), usize::MAX).users
}

fn walk(texts: &[&str], document: bool, current: (&str, &str), user_limit: usize) -> Candidates {
    let mut found = Candidates {
        current: (current.0.to_owned(), current.1.to_owned()),
        user_limit,
        ..Candidates::default()
    };
    for text in texts {
        collect_candidates(text, document, &mut found);
    }
    found
}

impl Candidates {
    /// Adds the references in `text[range]` that the renderer could link there, without collecting
    /// them first.
    pub(super) fn add_tokens(&mut self, text: &str, range: Range<usize>, opaque: &[Range<usize>]) {
        scan_tokens(text, range, opaque, |_, token| self.add(token));
    }

    fn add(&mut self, token: Token<'_>) {
        match token {
            // The page's own numbers and commits need no lookup.
            Token::Number(_) | Token::Commit(_) => {}
            Token::User(name) => {
                if self.users.len() < self.user_limit && self.seen_users.insert(name.to_owned()) {
                    self.users.push(name.to_owned());
                }
            }
            Token::RepoNumber { owner, repo, .. } | Token::RepoCommit { owner, repo, .. } => {
                self.add_repository(owner, repo);
            }
        }
    }

    fn add_repository(&mut self, owner: &str, repo: &str) {
        if (owner, repo) == (self.current.0.as_str(), self.current.1.as_str()) {
            return;
        }
        let known = self
            .repositories
            .iter()
            .any(|(known_owner, known_repo)| known_owner == owner && known_repo == repo);
        if !known && self.repositories.len() < MAX_REPOSITORIES {
            self.repositories.push((owner.to_owned(), repo.to_owned()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference tokens of `text[range]`, collected in order.
    fn tokens<'a>(
        text: &'a str,
        range: Range<usize>,
        opaque: &[Range<usize>],
    ) -> Vec<(Range<usize>, Token<'a>)> {
        let mut found = Vec::new();
        scan_tokens(text, range, opaque, |range, token| {
            found.push((range, token))
        });
        found
    }

    fn token(word: &str) -> Option<Token<'_>> {
        classify(word)
    }

    #[test]
    fn numbers_need_one_to_ten_digits_without_a_leading_zero() {
        assert_eq!(token("#12"), Some(Token::Number(12)));
        assert_eq!(token("#1"), Some(Token::Number(1)));
        assert_eq!(token("#9999999999"), Some(Token::Number(9_999_999_999)));
        for word in ["#0", "#012", "#12345678901", "#", "#1a", "#-1"] {
            assert_eq!(token(word), None, "{word}");
        }
    }

    #[test]
    fn users_are_names_without_dots_or_extra_marks() {
        assert_eq!(token("@alice"), Some(Token::User("alice")));
        assert_eq!(token("@a"), Some(Token::User("a")));
        assert_eq!(token("@user-name_1"), Some(Token::User("user-name_1")));
        for word in ["@user.name", "@types/node", "@@x", "@", "a@b.com"] {
            assert_eq!(token(word), None, "{word}");
        }
    }

    #[test]
    fn commit_shas_need_seven_to_forty_hex_digits_with_a_digit_and_a_letter() {
        assert_eq!(token("abcdef1"), Some(Token::Commit("abcdef1")));
        assert_eq!(
            token("0123456789abcdef0123456789abcdef01234567"),
            Some(Token::Commit("0123456789abcdef0123456789abcdef01234567"))
        );
        for word in [
            "1234567",                                                          // no letter
            "deadbeef",                                                         // no digit
            "abcdef",                                                           // too short
            "ABCDEF1",                                                          // capital letters
            "0123456789abcdef0123456789abcdef012345678",                        // 41 digits
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", // 64 digits
            "v1.2.3",
            "123e4567-e89b-12d3-a456-426614174000",
        ] {
            assert_eq!(token(word), None, "{word}");
        }
    }

    #[test]
    fn cross_repository_forms_name_an_owner_and_a_repository() {
        assert_eq!(
            token("alice/demo#12"),
            Some(Token::RepoNumber {
                owner: "alice",
                repo: "demo",
                number: 12
            })
        );
        assert_eq!(
            token("alice/demo.git@abcdef1"),
            Some(Token::RepoCommit {
                owner: "alice",
                repo: "demo.git",
                sha: "abcdef1"
            })
        );
        for word in [
            "alice/demo",
            "a/b/c#1",
            "/demo#1",
            "alice/#1",
            "foo#12",
            "x/y#0",
        ] {
            assert_eq!(token(word), None, "{word}");
        }
    }

    #[test]
    fn surrounding_punctuation_and_trailing_dots_are_not_part_of_a_token() {
        fn found(text: &str) -> Vec<(Range<usize>, Token<'_>)> {
            tokens(text, 0..text.len(), &[])
        }
        assert_eq!(found("fix #12."), vec![(4..7, Token::Number(12))]);
        assert_eq!(found("(#12)"), vec![(1..4, Token::Number(12))]);
        assert_eq!(found("@user,"), vec![(0..5, Token::User("user"))]);
        assert!(found("#12#13").is_empty());
        assert!(found("foo#12").is_empty());
    }

    #[test]
    fn a_word_with_an_opaque_byte_yields_no_token() {
        // `\#12 #3` where the escaped `#` at byte 0 is opaque.
        let text = "#12 #3";
        assert_eq!(
            tokens(text, 0..text.len(), std::slice::from_ref(&(0..1))),
            vec![(4..6, Token::Number(3))]
        );
    }

    #[test]
    fn links_need_the_target_to_exist() {
        let references = References {
            owner: "alice".into(),
            repo: "demo".into(),
            last_number: 5,
            users: vec!["alice".into(), "bob".into()],
            repositories: vec![Repository {
                owner: "carol".into(),
                name: "other".into(),
                last_number: 2,
            }],
        };
        let href = |token: Token<'_>| references.link(&token).map(|link| link.href);
        assert_eq!(
            href(Token::Number(5)).as_deref(),
            Some("/alice/demo/issues/5")
        );
        assert_eq!(href(Token::Number(6)), None);
        assert_eq!(href(Token::User("bob")).as_deref(), Some("/bob"));
        assert_eq!(href(Token::User("dave")), None);
        assert_eq!(
            href(Token::RepoNumber {
                owner: "carol",
                repo: "other",
                number: 2
            })
            .as_deref(),
            Some("/carol/other/issues/2")
        );
        assert_eq!(
            href(Token::RepoNumber {
                owner: "carol",
                repo: "secret",
                number: 1
            }),
            None
        );
        let commit = references.link(&Token::Commit("abcdef1234")).unwrap();
        assert_eq!(commit.href, "/alice/demo/commit/abcdef1234");
        assert_eq!(commit.text, "abcdef1");
        assert_eq!(commit.title.as_deref(), Some("abcdef1234"));
        assert_eq!(commit.class, COMMIT);
        assert!(References::default()
            .link(&Token::Commit("abcdef1"))
            .is_none());
    }

    #[test]
    fn candidates_collect_each_name_once_and_stay_capped() {
        let text = "#3 @bob @bob carol/x#1 carol/x@abcdef1 @alice";
        let found = candidates(&[text], false, ("alice", "demo"));
        assert_eq!(found.users, vec!["bob".to_owned(), "alice".to_owned()]);
        assert_eq!(
            found.repositories,
            vec![("carol".to_owned(), "x".to_owned())]
        );

        let many: Vec<String> = (0..MAX_USERS + 5).map(|n| format!("@user{n}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(
            candidates(&refs, false, ("alice", "demo")).users.len(),
            MAX_USERS
        );
    }

    #[test]
    fn code_spans_are_not_candidates() {
        // Link text may be a candidate too: the renderer skips it, which only costs a lookup.
        let found = candidates(&["`@code` and @text"], false, ("alice", "demo"));
        assert_eq!(found.users, vec!["text".to_owned()]);
    }

    /// `count` distinct usernames, written as prose writes them.
    fn usernames(count: usize) -> String {
        (0..count).map(|n| format!("@user{n} ")).collect()
    }

    /// Ten distinct repositories, written as prose writes them.
    fn repositories() -> String {
        (0..10).map(|n| format!("o{n}/r#1 ")).collect()
    }

    /// A page of `alice/demo` that resolved `bob` and nothing else.
    fn resolved_bob() -> References {
        References {
            owner: "alice".into(),
            repo: "demo".into(),
            last_number: 0,
            users: vec!["bob".into()],
            repositories: Vec::new(),
        }
    }

    /// Text inside a raw element takes no part in the budgets: the renderer links none of it, so
    /// the prose after the element still counts.
    fn budgets_past_a_raw_element(open: &str, close: &str) {
        let source = format!(
            "Text {open}{}{}{close} Thanks @bob and carol/other#9",
            usernames(50),
            repositories()
        );
        let found = candidates(&[source.as_str()], false, ("alice", "demo"));
        assert_eq!(found.users, vec!["bob".to_owned()], "{source}");
        assert_eq!(
            found.repositories,
            vec![("carol".to_owned(), "other".to_owned())],
            "{source}"
        );
    }

    #[test]
    fn raw_anchor_text_does_not_use_the_candidate_budgets() {
        budgets_past_a_raw_element("<a href=\"/x\">", "</a>");
    }

    #[test]
    fn raw_code_text_does_not_use_the_candidate_budgets() {
        budgets_past_a_raw_element("<code>", "</code>");
    }

    #[test]
    fn raw_pre_text_does_not_use_the_candidate_budgets() {
        budgets_past_a_raw_element("<pre>", "</pre>");
    }

    /// An element left open ends with its paragraph, so the next paragraph links as usual. The
    /// renderer agrees.
    #[test]
    fn an_unclosed_raw_element_ends_with_its_paragraph() {
        let source = format!("Text <code>{}\n\nThanks @bob", usernames(50));
        let found = candidates(&[source.as_str()], false, ("alice", "demo"));
        assert_eq!(found.users, vec!["bob".to_owned()]);
        let html = super::super::markdown::parse_markdown(&source, &resolved_bob());
        assert!(html.contains("href=\"/bob\""), "{html}");
    }

    /// A closing tag that matches no open element is text, so the element stays open and the prose
    /// after it is inside the element. The renderer links none of it, and no candidate counts it.
    #[test]
    fn a_mismatched_closing_tag_leaves_the_element_open() {
        let source = format!("Text <code>{}</pre> Thanks @bob", usernames(50));
        let found = candidates(&[source.as_str()], false, ("alice", "demo"));
        assert!(found.users.is_empty(), "{:?}", found.users);
        let html = super::super::markdown::parse_markdown(&source, &resolved_bob());
        assert!(!html.contains("href=\"/bob\""), "{html}");
    }

    /// The page's own repository takes no place among the other repositories, so ten others still
    /// resolve when the page names its own repository first.
    #[test]
    fn the_current_repository_takes_no_place_among_the_other_repositories() {
        let source = format!("alice/demo#1 {}", repositories());
        let found = candidates(&[source.as_str()], false, ("alice", "demo"));
        assert_eq!(found.repositories.len(), 10, "{:?}", found.repositories);
        assert!(found.repositories.iter().all(|(owner, _)| owner != "alice"));
    }

    /// Escaped names are text the page shows as written, so they take no place among the users.
    #[test]
    fn escaped_names_take_no_place_among_the_users() {
        let escaped: Vec<String> = (0..MAX_USERS).map(|n| format!("\\@user{n}")).collect();
        let source = format!("{} @bob", escaped.join(" "));
        let found = candidates(&[source.as_str()], false, ("alice", "demo"));
        assert_eq!(found.users, vec!["bob".to_owned()]);
    }

    /// Entity-encoded repositories are text the page shows as written, so they take no place among
    /// the repositories.
    #[test]
    fn encoded_repositories_take_no_place_among_the_repositories() {
        let encoded: Vec<String> = (0..MAX_REPOSITORIES)
            .map(|n| format!("owner{n}&#47;repo{n}#1"))
            .collect();
        let source = format!("{} carol/other#9", encoded.join(" "));
        let found = candidates(&[source.as_str()], false, ("alice", "demo"));
        assert_eq!(
            found.repositories,
            vec![("carol".to_owned(), "other".to_owned())]
        );
    }

    /// A paragraph of 50 footnote labels `[^@userN]` before `Thanks @bob`, with a note for each label.
    /// A document renders the labels as footnote references, and a comment renders them as text.
    fn footnoted_users() -> String {
        let labels: String = (0..MAX_USERS).map(|n| format!("[^@user{n}] ")).collect();
        let notes: String = (0..MAX_USERS)
            .map(|n| format!("[^@user{n}]: note {n}\n\n"))
            .collect();
        format!("{labels}Thanks @bob\n\n{notes}")
    }

    /// In a document a footnote label is a reference, so the 50 labels take no place among the users.
    #[test]
    fn footnote_labels_take_no_place_among_the_users_in_a_document() {
        let source = footnoted_users();
        let found = candidates(&[source.as_str()], true, ("alice", "demo"));
        assert_eq!(found.users, vec!["bob".to_owned()]);
    }

    /// In a comment a footnote label is text, so its names are candidates and render as links.
    #[test]
    fn footnote_labels_are_candidates_in_a_comment() {
        let source = footnoted_users();
        let found = candidates(&[source.as_str()], false, ("alice", "demo"));
        for n in 0..10 {
            assert!(
                found.users.contains(&format!("user{n}")),
                "{:?}",
                found.users
            );
        }
        let resolved = References {
            owner: "alice".into(),
            repo: "demo".into(),
            last_number: 0,
            users: found.users,
            repositories: Vec::new(),
        };
        let html = super::super::markdown::parse_markdown(&source, &resolved);
        assert!(html.contains("href=\"/user0\""), "{html}");
    }

    /// In a document, repository-shaped footnote labels are references, so they take no place among
    /// the repositories.
    #[test]
    fn footnote_labels_take_no_place_among_the_repositories_in_a_document() {
        let labels: String = (0..MAX_REPOSITORIES)
            .map(|n| format!("[^o{n}/r#1] "))
            .collect();
        let notes: String = (0..MAX_REPOSITORIES)
            .map(|n| format!("[^o{n}/r#1]: note {n}\n\n"))
            .collect();
        let source = format!("{labels}See carol/other#9\n\n{notes}");
        let found = candidates(&[source.as_str()], true, ("alice", "demo"));
        assert_eq!(
            found.repositories,
            vec![("carol".to_owned(), "other".to_owned())]
        );
    }

    /// A file-editor-sized document whose render output would pass the output budget, with a
    /// reference after it. The candidate walk keeps no output, so the budget cannot end it early,
    /// and the source stays under its own limit.
    #[test]
    fn a_walk_past_the_output_budget_still_finds_later_references() {
        let quotes = "\"".repeat(60 * 1024);
        let blocks: String = (0..4)
            .map(|_| format!("```unknown\n{quotes}\n```\n\n"))
            .collect();
        let links = "[x](/x) ".repeat(76_000);
        let source = format!("{blocks}{links}\n\nFinal @bob");
        assert!(source.len() < 1024 * 1024, "{}", source.len());
        let found = candidates(&[source.as_str()], true, ("alice", "demo"));
        assert_eq!(found.users, vec!["bob".to_owned()]);
    }

    /// Tokenizing hostile input takes about as long per byte at four times the size.
    #[test]
    fn classification_stays_linear_on_hostile_input() {
        for (unit, count) in [
            ("#1 ", 100_000),
            ("@a ", 100_000),
            ("0123456789abcdef0123456789abcdef01234567 ", 50_000),
        ] {
            let small = unit.repeat(count);
            let large = unit.repeat(count * 4);
            // The fastest of three runs: other work on the machine only adds time.
            let time = |text: &str| {
                (0..3)
                    .map(|_| {
                        let started = std::time::Instant::now();
                        let found = tokens(text, 0..text.len(), &[]);
                        std::hint::black_box(found);
                        started.elapsed()
                    })
                    .min()
                    .unwrap()
            };
            let (one, four) = (time(&small), time(&large));
            assert!(
                four < one * 12 + std::time::Duration::from_millis(50),
                "{unit:?} x{count}: {one:?} then {four:?}"
            );
        }
    }
}
