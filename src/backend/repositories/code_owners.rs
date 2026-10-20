//! CODEOWNERS: which owners a path has at a commit, and which of them may review it.
//! Parsing and matching are pure. Resolving owners reads users and teams.

use crate::backend::ci::workflows::{code_owner_glob_matches_bounded, WorkflowMatchBudget};
use crate::backend::cluster::NodeContext;
use crate::backend::db;
use crate::backend::db::models::Repository;
use crate::backend::repositories::store::{repo_store, BlobRange, ErrorKind, RepoError};
use std::collections::{HashMap, HashSet};

/// Where GitHub looks, in its order. The first file found is the one used.
const LOCATIONS: [&str; 3] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];
/// GitHub reads at most this many bytes of the file.
const MAX_BYTES: usize = 3 * 1024 * 1024;
/// Lines past this one are ignored.
const MAX_LINES: usize = 5_000;
const MAX_PATTERN_BYTES: usize = 1024;
const MAX_OWNERS: usize = 1000;
const MAX_OWNER_BYTES: usize = 320;
const MAX_OWNER_EXPANSIONS: usize = 10_000;
pub(crate) const MAX_REVIEWERS: usize = 5000;

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
    pub fn owners_for<'a>(
        &'a self,
        path: &str,
        budget: &mut WorkflowMatchBudget,
    ) -> anyhow::Result<&'a [String]> {
        for entry in self.entries.iter().rev() {
            if entry.matches(path, budget)? {
                return Ok(&entry.owners);
            }
        }
        Ok(&[])
    }

    /// Each changed path that has owners, with those owners as written.
    pub fn owned_paths(&self, changed: &[String]) -> anyhow::Result<Vec<(String, &[String])>> {
        let mut budget = WorkflowMatchBudget::new();
        let mut paths = Vec::new();
        let mut expanded = 0usize;
        for path in changed {
            let owners = self.owners_for(path, &mut budget)?;
            if !owners.is_empty() {
                expanded = expanded.saturating_add(owners.len());
                anyhow::ensure!(
                    expanded <= MAX_OWNER_EXPANSIONS,
                    "CODEOWNERS owner expansion exceeds its limit"
                );
                paths.push((path.clone(), owners));
            }
        }
        Ok(paths)
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
    fn matches(&self, path: &str, budget: &mut WorkflowMatchBudget) -> anyhow::Result<bool> {
        let parts: Vec<&str> = path.split('/').collect();
        let longest = if self.dir_only {
            parts.len() - 1
        } else {
            parts.len()
        };
        for len in 1..=longest {
            if segments_match(&self.segments, &parts[..len], budget)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Whether the pattern segments match the whole path segments. `**` takes zero or more of them.
/// Work is the product of the two lengths, so a long pattern cannot blow up.
fn segments_match(
    pattern: &[String],
    path: &[&str],
    budget: &mut WorkflowMatchBudget,
) -> anyhow::Result<bool> {
    let mut next = vec![false; path.len() + 1];
    next[path.len()] = true;
    for segment in pattern.iter().rev() {
        let mut row = vec![false; path.len() + 1];
        for index in (0..=path.len()).rev() {
            code_owner_glob_matches_bounded("", "", budget)?;
            row[index] = if segment == "**" {
                next[index] || (index < path.len() && row[index + 1])
            } else {
                index < path.len()
                    && next[index + 1]
                    && code_owner_glob_matches_bounded(segment, path[index], budget)?
            };
        }
        next = row;
    }
    Ok(next[0])
}

/// Reads a CODEOWNERS file. A line with no owners removes them, and a line that names no path is
/// skipped. Owners are the tokens with an `@` in them, before any `#` that starts a comment.
pub fn parse(text: &str) -> anyhow::Result<Rules> {
    let mut entries = Vec::new();
    let mut owner_count = 0usize;
    for line in text
        .lines()
        .take(MAX_LINES)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let mut escaped = false;
        let end = line
            .char_indices()
            .find_map(|(index, ch)| {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch.is_whitespace() {
                    return Some(index);
                }
                None
            })
            .unwrap_or(line.len());
        let pattern = &line[..end];
        anyhow::ensure!(
            pattern.len() <= MAX_PATTERN_BYTES,
            "CODEOWNERS pattern exceeds its size limit"
        );
        let mut owners = Vec::new();
        let mut seen = HashSet::new();
        for owner in line[end..]
            .split_whitespace()
            .take_while(|token| !token.starts_with('#'))
            .filter(|token| token.contains('@'))
        {
            anyhow::ensure!(
                owner.len() <= MAX_OWNER_BYTES,
                "CODEOWNERS owner exceeds its size limit"
            );
            if seen.insert(owner) {
                owner_count += 1;
                anyhow::ensure!(
                    owner_count <= MAX_OWNERS,
                    "CODEOWNERS owners exceed their limit"
                );
                owners.push(owner.to_owned());
            }
        }
        if let Some(entry) = Entry::compile(pattern, owners) {
            entries.push(entry);
        }
    }
    Ok(Rules { entries })
}

/// The CODEOWNERS of `commit`, or no rules when the repository has none.
pub async fn load_rules(
    repository: &Repository,
    cluster: Option<&NodeContext>,
    commit: &str,
) -> Result<Rules, RepoError> {
    let store = repo_store(repository, cluster);
    for location in LOCATIONS {
        match store
            .blob_range(
                commit,
                location,
                BlobRange::Window {
                    offset: 0,
                    length: MAX_BYTES as u64,
                },
            )
            .await
        {
            Ok(blob) => {
                let end = blob.bytes.len().min(MAX_BYTES);
                return parse(&String::from_utf8_lossy(&blob.bytes[..end])).map_err(|_| {
                    RepoError::new(
                        ErrorKind::Invalid,
                        "CODEOWNERS exceeds its size or work limit",
                    )
                });
            }
            Err(error) if error.kind == ErrorKind::MissingPath => continue,
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
    let (mut names, mut emails, mut teams) = (Vec::new(), Vec::new(), Vec::new());
    for owner in owners {
        if resolved.contains_key(owner) {
            continue;
        }
        anyhow::ensure!(
            owner.len() <= MAX_OWNER_BYTES,
            "CODEOWNERS owner exceeds its size limit"
        );
        anyhow::ensure!(
            resolved.len() < MAX_OWNERS,
            "CODEOWNERS has too many distinct owners"
        );
        resolved.insert(owner.to_owned(), Vec::new());
        match owner.strip_prefix('@') {
            Some(name) if name.contains('/') => teams.push(name.to_owned()),
            Some(name) => names.push(name.to_owned()),
            None => emails.push(owner.to_owned()),
        }
    }
    for names in names.chunks(500) {
        for user in db::users::accounts::get_users_by_usernames(pool, names).await? {
            resolved
                .get_mut(&format!("@{}", user.username))
                .expect("requested owner")
                .push(user.id);
        }
    }
    for emails in emails.chunks(500) {
        for user in db::users::accounts::get_users_by_emails(pool, emails).await? {
            if !user.email_verified {
                continue;
            }
            if let Some(ids) = user
                .email
                .as_ref()
                .and_then(|email| resolved.get_mut(email))
            {
                ids.push(user.id);
            }
        }
    }
    let mut members = 0;
    for teams in teams.chunks(500) {
        let Some(org_id) = repository.owner_org_id else {
            break;
        };
        for (team, id) in
            db::organizations::teams::team_members_by_names(pool, org_id, teams).await?
        {
            members += 1;
            anyhow::ensure!(
                members <= MAX_REVIEWERS,
                "CODEOWNERS resolves to too many team members"
            );
            resolved
                .get_mut(&format!("@{team}"))
                .expect("requested team")
                .push(id);
        }
    }
    let mut ids: Vec<i64> = resolved.values().flatten().copied().collect();
    ids.sort_unstable();
    ids.dedup();
    anyhow::ensure!(
        ids.len() <= MAX_REVIEWERS,
        "CODEOWNERS resolves to too many reviewers"
    );
    let mut writers = std::collections::HashSet::new();
    if !repository.is_archived {
        for ids in ids.chunks(500) {
            let levels: HashMap<_, _> =
                db::repositories::access::get_users_repo_access(pool, ids, repository.id)
                    .await?
                    .into_iter()
                    .collect();
            for user in db::users::accounts::get_users_by_ids(pool, ids).await? {
                if !user.is_banned
                    && !user.must_change_password
                    && ((repository.owner_kind == "user"
                        && (user.is_admin || repository.is_personal_owner(user.id)))
                        || levels.get(&user.id).is_some_and(|level| level.can_write()))
                {
                    writers.insert(user.id);
                }
            }
        }
    }
    for ids in resolved.values_mut() {
        ids.retain(|id| writers.contains(id));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners_of(text: &str, path: &str) -> Vec<String> {
        parse(text)
            .unwrap()
            .owners_for(path, &mut WorkflowMatchBudget::new())
            .unwrap()
            .to_vec()
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
    fn a_hash_ends_the_owners_on_its_line() {
        let text = "/x @alice # contact @bob\n/y @carol #@dave\n";
        assert_eq!(owners_of(text, "x"), vec!["@alice"]);
        assert_eq!(owners_of(text, "y"), vec!["@carol"]);
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
    fn escaped_spaces_and_wildcards_match_literal_path_characters() {
        let text = r"/docs/Project\ Plan.md @bob
/docs/literal\*.md @carol";
        assert_eq!(owners_of(text, "docs/Project Plan.md"), ["@bob"]);
        assert_eq!(owners_of(text, "docs/literal*.md"), ["@carol"]);
        assert!(owners_of(text, "docs/literalX.md").is_empty());
    }

    #[test]
    fn oversized_patterns_are_errors_not_missing_owners() {
        assert!(parse(&format!("{} @bob", "x".repeat(MAX_PATTERN_BYTES + 1))).is_err());
    }

    #[test]
    fn all_paths_share_a_match_budget_and_exhaustion_is_an_error() {
        let text = format!("{}Z @bob\n", "*x".repeat(200));
        let rules = parse(&text).unwrap();
        assert!(rules.owned_paths(&vec!["x".repeat(255); 1000]).is_err());
    }

    #[test]
    fn owners_are_deduplicated_and_expansion_is_bounded() {
        let rules = parse(&format!("* {}", "@alice ".repeat(100_000))).unwrap();
        assert_eq!(
            rules.owned_paths(&vec!["a".into(); 1000]).unwrap().len(),
            1000
        );
        let owners = (0..101)
            .map(|n| format!("@user{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let rules = parse(&format!("* {owners}")).unwrap();
        assert!(rules.owned_paths(&vec!["a".into(); 100]).is_err());
        let owners = (0..=MAX_OWNERS)
            .map(|n| format!("@user{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(parse(&format!("* {owners}")).is_err());
        assert!(parse(&format!("* @{}", "x".repeat(MAX_OWNER_BYTES))).is_err());
    }

    #[test]
    fn repeated_owners_are_deduplicated_before_expansion() {
        let rules = parse(&format!("* {}", "@bob ".repeat(500_000))).unwrap();
        let paths = rules.owned_paths(&vec!["file".to_owned(); 8192]).unwrap();
        assert_eq!(paths.len(), 8192);
        assert!(paths.iter().all(|(_, owners)| *owners == ["@bob"]));
    }

    #[test]
    fn owner_expansion_is_bounded_before_cloning() {
        let owners = (0..1000)
            .map(|id| format!("@owner{id} "))
            .collect::<String>();
        let rules = parse(&format!("* {owners}")).unwrap();
        assert!(rules.owned_paths(&vec!["file".to_owned(); 8192]).is_err());
    }

    #[test]
    fn owned_paths_lists_only_changed_files_with_owners() {
        let rules = parse("/src/ @dev\n").unwrap();
        let changed = vec!["src/a.rs".to_owned(), "README.md".to_owned()];
        assert_eq!(
            rules.owned_paths(&changed).unwrap(),
            vec![("src/a.rs".to_owned(), ["@dev".to_owned()].as_slice())]
        );
    }
}
