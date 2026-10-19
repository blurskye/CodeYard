//! CODEOWNERS: which owners a path has at a commit, and which of them may review it.
//! Parsing and matching are pure. Resolving owners reads users and teams.

use crate::backend::ci::workflows::github_glob_matches;
use crate::backend::cluster::NodeContext;
use crate::backend::db;
use crate::backend::db::models::Repository;
use crate::backend::repositories::access::can_write;
use crate::backend::repositories::store::{repo_store, ErrorKind, RepoError};
use std::collections::HashMap;

/// Where GitHub looks, in its order. The first file found is the one used.
const LOCATIONS: [&str; 3] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];
/// GitHub reads at most this many bytes of the file.
const MAX_BYTES: usize = 3 * 1024 * 1024;
/// Lines past this one are ignored.
const MAX_LINES: usize = 5_000;

/// One line of the file: a compiled path pattern and the owners it names.
struct Entry {
    /// Path segments; `**` crosses directories. A pattern without a slash is `**/name`.
    segments: Vec<String>,
    /// A trailing slash names a directory, so only files below it match.
    dir_only: bool,
    /// Written as `@user`, `@org/team` or an email. An empty list removes the owners above it.
    owners: Vec<String>,
}

#[derive(Default)]
pub struct Rules {
    entries: Vec<Entry>,
}

impl Rules {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The owners of `path`, from the last line whose pattern matches it.
    pub fn owners_for(&self, path: &str) -> &[String] {
        match self.entries.iter().rev().find(|entry| entry.matches(path)) {
            Some(entry) => &entry.owners,
            None => &[],
        }
    }

    /// Each changed path that has owners, with those owners as written.
    pub fn owned_paths(&self, changed: &[String]) -> Vec<(String, Vec<String>)> {
        if self.is_empty() {
            return Vec::new();
        }
        changed
            .iter()
            .filter_map(|path| {
                let owners = self.owners_for(path);
                (!owners.is_empty()).then(|| (path.clone(), owners.to_vec()))
            })
            .collect()
    }
}

impl Entry {
    fn compile(pattern: &str, owners: Vec<String>) -> Option<Self> {
        let dir_only = pattern.ends_with('/');
        let body = pattern.trim_end_matches('/');
        // A slash anywhere but the end makes the pattern relative to the root, as in gitignore.
        let anchored = body.contains('/');
        let body = body.trim_start_matches('/');
        if body.is_empty() {
            return None;
        }
        let mut segments = Vec::new();
        if !anchored {
            segments.push("**".to_owned());
        }
        for segment in body.split('/').filter(|segment| !segment.is_empty()) {
            // Consecutive `**` segments match the same paths as one, so keep one.
            if segment == "**" && segments.last().is_some_and(|last| last == "**") {
                continue;
            }
            segments.push(segment.to_owned());
        }
        Some(Self {
            segments,
            dir_only,
            owners,
        })
    }

    /// A pattern matches a file, or a directory that holds it: `docs` names everything under docs.
    fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        let longest = if self.dir_only {
            parts.len() - 1
        } else {
            parts.len()
        };
        (1..=longest).any(|len| segments_match(&self.segments, &parts[..len]))
    }
}

/// Whether the pattern segments match the whole path segments. `**` takes zero or more of them.
/// Work is the product of the two lengths, so a long pattern cannot blow up.
fn segments_match(pattern: &[String], path: &[&str]) -> bool {
    let mut next = vec![false; path.len() + 1];
    next[path.len()] = true;
    for segment in pattern.iter().rev() {
        let mut row = vec![false; path.len() + 1];
        for index in (0..=path.len()).rev() {
            row[index] = if segment == "**" {
                next[index] || (index < path.len() && row[index + 1])
            } else {
                index < path.len() && github_glob_matches(segment, path[index]) && next[index + 1]
            };
        }
        next = row;
    }
    next[0]
}

/// Reads a CODEOWNERS file. A line with no owners removes them, and a line that names no path is
/// skipped. Owners are the tokens with an `@` in them.
pub fn parse(text: &str) -> Rules {
    let entries = text
        .lines()
        .take(MAX_LINES)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut tokens = line.split_whitespace();
            let pattern = tokens.next()?;
            let owners = tokens
                .filter(|token| token.contains('@'))
                .map(str::to_owned)
                .collect();
            Entry::compile(pattern, owners)
        })
        .collect();
    Rules { entries }
}

/// The CODEOWNERS of `commit`, or no rules when the repository has none.
pub async fn load_rules(
    repository: &Repository,
    cluster: Option<&NodeContext>,
    commit: &str,
) -> Result<Rules, RepoError> {
    let store = repo_store(repository, cluster);
    for location in LOCATIONS {
        match store.blob_bytes(commit, location).await {
            Ok(blob) => {
                let end = blob.bytes.len().min(MAX_BYTES);
                return Ok(parse(&String::from_utf8_lossy(&blob.bytes[..end])));
            }
            Err(error) if error.kind == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(Rules::default())
}

/// The users with write access that each written owner names, keyed by the owner as written.
/// `@org/team` names the team's members, `@user` one user, and an email the user it belongs to.
/// An owner who names nobody, or nobody who can write, maps to an empty list.
pub async fn resolve_owners<'a>(
    pool: &db::DbPool,
    repository: &Repository,
    owners: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<HashMap<String, Vec<i64>>> {
    let mut resolved = HashMap::new();
    for owner in owners {
        if resolved.contains_key(owner) {
            continue;
        }
        let mut ids = Vec::new();
        if let Some(name) = owner.strip_prefix('@') {
            match name.split_once('/') {
                Some((org, slug)) => {
                    ids = db::organizations::teams::team_member_user_ids(pool, org, slug).await?
                }
                None => {
                    if let Some(user) =
                        db::users::accounts::get_user_by_username(pool, name).await?
                    {
                        ids.push(user.id);
                    }
                }
            }
        } else if let Some(user) = db::users::accounts::get_user_by_email(pool, owner).await? {
            ids.push(user.id);
        }
        let mut writers = Vec::new();
        for id in ids {
            if let Some(user) = db::users::accounts::get_user_by_id(pool, id).await? {
                if can_write(pool, &user, repository).await? {
                    writers.push(user.id);
                }
            }
        }
        resolved.insert(owner.to_owned(), writers);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners_of(text: &str, path: &str) -> Vec<String> {
        parse(text).owners_for(path).to_vec()
    }

    #[test]
    fn the_last_matching_line_wins_and_an_empty_line_removes_owners() {
        let text = "* @everyone\n/docs/ @docs\n/docs/private/ \n";
        assert_eq!(owners_of(text, "src/main.rs"), vec!["@everyone"]);
        assert_eq!(owners_of(text, "docs/guide.md"), vec!["@docs"]);
        assert!(owners_of(text, "docs/private/key.txt").is_empty());
    }

    #[test]
    fn a_pattern_without_a_slash_matches_at_any_depth() {
        let text = "*.js @web\nlogo.png @design\n";
        assert_eq!(owners_of(text, "app.js"), vec!["@web"]);
        assert_eq!(owners_of(text, "ui/widgets/app.js"), vec!["@web"]);
        assert_eq!(owners_of(text, "ui/logo.png"), vec!["@design"]);
        assert!(owners_of(text, "ui/app.jsx").is_empty());
    }

    #[test]
    fn a_leading_slash_anchors_the_pattern_to_the_root() {
        let text = "/build.rs @infra\n";
        assert_eq!(owners_of(text, "build.rs"), vec!["@infra"]);
        assert!(owners_of(text, "crates/x/build.rs").is_empty());
        // A slash in the middle anchors too, as in gitignore.
        let text = "docs/api @api\n";
        assert_eq!(owners_of(text, "docs/api/index.md"), vec!["@api"]);
        assert!(owners_of(text, "vendor/docs/api/index.md").is_empty());
    }

    #[test]
    fn a_trailing_slash_names_a_directory_and_everything_under_it() {
        let text = "docs/ @docs\n";
        assert_eq!(owners_of(text, "docs/a/b.md"), vec!["@docs"]);
        // A file called `docs` is not inside a directory of that name.
        assert!(owners_of(text, "docs").is_empty());
        // Without the slash, the name matches the file and the directory alike.
        assert_eq!(owners_of("docs @docs\n", "docs"), vec!["@docs"]);
        assert_eq!(owners_of("docs @docs\n", "docs/a.md"), vec!["@docs"]);
    }

    #[test]
    fn double_star_crosses_directories_and_single_star_does_not() {
        let text = "/src/*.rs @one\n/src/**/gen.rs @two\n/**/tests/** @three\n";
        assert_eq!(owners_of(text, "src/lib.rs"), vec!["@one"]);
        assert!(owners_of(text, "src/a/lib.rs").is_empty());
        assert_eq!(owners_of(text, "src/gen.rs"), vec!["@two"]);
        assert_eq!(owners_of(text, "src/a/b/gen.rs"), vec!["@two"]);
        assert_eq!(owners_of(text, "x/y/tests/t.rs"), vec!["@three"]);
    }

    #[test]
    fn team_and_email_owners_are_kept_as_written_and_other_tokens_are_not_owners() {
        let text = "/a.txt @org/docs ada@example.com not-an-owner @alice\n";
        assert_eq!(
            owners_of(text, "a.txt"),
            vec!["@org/docs", "ada@example.com", "@alice"]
        );
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let text = "# a comment\n\n   \n/x @x\n";
        assert_eq!(owners_of(text, "x"), vec!["@x"]);
    }

    #[test]
    fn only_lines_within_the_line_cap_count() {
        let mut text = String::new();
        for _ in 0..MAX_LINES {
            text.push_str("# filler\n");
        }
        text.push_str("/late.txt @late\n");
        assert!(owners_of(&text, "late.txt").is_empty());
    }

    #[test]
    fn owned_paths_lists_only_changed_files_with_owners() {
        let rules = parse("/src/ @dev\n");
        let changed = vec!["src/a.rs".to_owned(), "README.md".to_owned()];
        assert_eq!(
            rules.owned_paths(&changed),
            vec![("src/a.rs".to_owned(), vec!["@dev".to_owned()])]
        );
    }
}
