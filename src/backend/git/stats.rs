use super::repository::{peel_to_commit_checked, process, ObjectTooLarge};
use anyhow::{ensure, Result};
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageStat {
    pub language: String,
    pub bytes: u64,
    pub percentage: f32,
    pub color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContributorInfo {
    pub name: String,
    pub email: String,
    pub commits: u32,
}

static LANGUAGE_COLORS: &[(&str, &str)] = &[
    ("Rust", "#dea584"),
    ("JavaScript", "#f1e05a"),
    ("TypeScript", "#3178c6"),
    ("Python", "#3572A5"),
    ("Go", "#00ADD8"),
    ("Java", "#b07219"),
    ("C", "#555555"),
    ("C++", "#f34b7d"),
    ("C#", "#178600"),
    ("Ruby", "#701516"),
    ("PHP", "#4F5D95"),
    ("Swift", "#F05138"),
    ("Kotlin", "#A97BFF"),
    ("Scala", "#c22d40"),
    ("HTML", "#e34c26"),
    ("CSS", "#563d7c"),
    ("SCSS", "#c6538c"),
    ("Shell", "#89e051"),
    ("Markdown", "#083fa1"),
    ("JSON", "#292929"),
    ("YAML", "#cb171e"),
    ("TOML", "#9c4221"),
    ("SQL", "#e38c00"),
    ("Dockerfile", "#384d54"),
    ("Vue", "#41b883"),
    ("Svelte", "#ff3e00"),
];

static EXTENSION_MAP: &[(&str, &str)] = &[
    ("rs", "Rust"),
    ("js", "JavaScript"),
    ("mjs", "JavaScript"),
    ("cjs", "JavaScript"),
    ("ts", "TypeScript"),
    ("tsx", "TypeScript"),
    ("jsx", "JavaScript"),
    ("py", "Python"),
    ("go", "Go"),
    ("java", "Java"),
    ("c", "C"),
    ("h", "C"),
    ("cpp", "C++"),
    ("cxx", "C++"),
    ("cc", "C++"),
    ("hpp", "C++"),
    ("cs", "C#"),
    ("rb", "Ruby"),
    ("php", "PHP"),
    ("swift", "Swift"),
    ("kt", "Kotlin"),
    ("kts", "Kotlin"),
    ("scala", "Scala"),
    ("html", "HTML"),
    ("htm", "HTML"),
    ("css", "CSS"),
    ("scss", "SCSS"),
    ("sass", "SCSS"),
    ("sh", "Shell"),
    ("bash", "Shell"),
    ("zsh", "Shell"),
    ("md", "Markdown"),
    ("json", "JSON"),
    ("yaml", "YAML"),
    ("yml", "YAML"),
    ("toml", "TOML"),
    ("sql", "SQL"),
    ("vue", "Vue"),
    ("svelte", "Svelte"),
];

fn get_language_color(language: &str) -> String {
    LANGUAGE_COLORS
        .iter()
        .find(|(l, _)| *l == language)
        .map(|(_, c)| c.to_string())
        .unwrap_or_else(|| "#8b949e".to_string())
}

fn extension_to_language(ext: &str) -> Option<&'static str> {
    EXTENSION_MAP
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, l)| *l)
}

pub const MAX_LANGUAGE_FILES: usize = 100_000;
const MAX_LANGUAGE_PATH_BYTES: usize = 4 * 1024;
const MAX_LANGUAGE_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

fn check_language_file_count(count: usize) -> Result<()> {
    if count > MAX_LANGUAGE_FILES {
        return Err(ObjectTooLarge.into());
    }
    Ok(())
}

fn parse_language_stats(output: &[u8]) -> Result<Vec<LanguageStat>> {
    let mut lang_bytes: HashMap<String, u64> = HashMap::new();
    let mut total_bytes = 0u64;
    let mut file_count = 0usize;
    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        file_count += 1;
        check_language_file_count(file_count)?;
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| anyhow::anyhow!("Malformed Git tree record"))?;
        let (header, filepath) = record.split_at(tab);
        let filepath = &filepath[1..];
        if filepath.len() > MAX_LANGUAGE_PATH_BYTES {
            return Err(ObjectTooLarge.into());
        }
        let mut fields = header
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        let _mode = fields.next();
        let kind = fields.next();
        let _object = fields.next();
        let size = fields.next();
        if kind != Some(b"blob".as_slice()) {
            continue;
        }
        let size = size.ok_or_else(|| anyhow::anyhow!("Missing Git blob size"))?;
        let size = std::str::from_utf8(size)?.parse::<u64>()?;
        let filepath = String::from_utf8_lossy(filepath);
        let path = Path::new(filepath.as_ref());
        let language = if path.file_name().is_some_and(|name| name == "Dockerfile") {
            Some("Dockerfile")
        } else {
            path.extension()
                .and_then(|extension| extension.to_str())
                .and_then(extension_to_language)
        };
        if let Some(language) = language {
            let bytes = lang_bytes.entry(language.to_owned()).or_insert(0);
            *bytes = bytes
                .checked_add(size)
                .filter(|bytes| *bytes <= i64::MAX as u64)
                .ok_or(ObjectTooLarge)?;
            total_bytes = total_bytes
                .checked_add(size)
                .filter(|bytes| *bytes <= i64::MAX as u64)
                .ok_or(ObjectTooLarge)?;
        }
    }

    if total_bytes == 0 {
        return Ok(Vec::new());
    }
    let mut stats: Vec<_> = lang_bytes
        .into_iter()
        .map(|(language, bytes)| LanguageStat {
            percentage: (bytes as f64 / total_bytes as f64 * 100.0) as f32,
            color: get_language_color(&language),
            language,
            bytes,
        })
        .collect();
    stats.sort_by_key(|stat| std::cmp::Reverse(stat.bytes));
    Ok(stats)
}

/// True when the branch does not exist. A branch that points at a missing object is
/// a damaged repository and stays an error.
fn is_not_found(error: &anyhow::Error) -> bool {
    error.downcast_ref::<git2::Error>().is_some_and(|error| {
        error.code() == git2::ErrorCode::NotFound && error.class() == git2::ErrorClass::Reference
    })
}

/// The commit `branch` points at, or `None` while the branch is unborn.
pub(crate) fn branch_tip(repo_path: &Path, branch: &str) -> Result<Option<String>> {
    let repo = Repository::open_bare(repo_path)?;
    let tip = match peel_to_commit_checked(&repo, &format!("refs/heads/{branch}")) {
        Ok(commit) => Some(commit.id().to_string()),
        Err(error) if is_not_found(&error) => None,
        Err(error) => return Err(error),
    };
    Ok(tip)
}

pub(crate) async fn compute_language_stats(
    repo_path: &Path,
    git_ref: &str,
) -> Result<Vec<LanguageStat>> {
    let repo = Repository::open_bare(repo_path)?;
    let revision = match peel_to_commit_checked(&repo, git_ref) {
        Ok(commit) => commit.id().to_string(),
        // An unborn default branch is an empty repository, not an error.
        Err(error) if is_not_found(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    drop(repo);
    let mut command = process::git_command()?;
    command
        .args(["ls-tree", "-r", "-l", "-z", &revision, "--"])
        .current_dir(repo_path);
    let (status, output) = process::buffered(
        command,
        b"",
        MAX_LANGUAGE_OUTPUT_BYTES,
        process::RPC_TIMEOUT,
    )
    .await?;
    ensure!(status.success(), "Git language listing failed");
    parse_language_stats(&output)
}

pub const MAX_CONTRIBUTORS: usize = 10_000;
const MAX_CONTRIBUTOR_LINE_BYTES: usize = 4 * 1024;
const MAX_CONTRIBUTOR_OUTPUT_BYTES: usize = 1024 * 1024;

fn check_contributor_count(count: usize) -> Result<()> {
    if count > MAX_CONTRIBUTORS {
        return Err(ObjectTooLarge.into());
    }
    Ok(())
}

fn parse_contributors(output: &[u8]) -> Result<Vec<ContributorInfo>> {
    let mut contributors = Vec::new();
    for line in String::from_utf8_lossy(output).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_CONTRIBUTOR_LINE_BYTES {
            return Err(ObjectTooLarge.into());
        }
        check_contributor_count(contributors.len() + 1)?;
        let (commits, author) = line
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("Malformed Git contributor record"))?;
        let commits = commits
            .trim()
            .parse::<u64>()
            .ok()
            .and_then(|commits| u32::try_from(commits).ok())
            .filter(|commits| *commits <= i32::MAX as u32)
            .ok_or(ObjectTooLarge)?;
        let author = author.trim();
        let email_start = author
            .find('<')
            .ok_or_else(|| anyhow::anyhow!("Missing Git contributor email"))?;
        let email_end = author[email_start + 1..]
            .find('>')
            .ok_or_else(|| anyhow::anyhow!("Malformed Git contributor email"))?;
        let email_end = email_start + 1 + email_end;
        contributors.push(ContributorInfo {
            name: author[..email_start].trim().to_owned(),
            email: author[email_start + 1..email_end].to_owned(),
            commits,
        });
    }
    Ok(contributors)
}

/// A result read from history, kept until a ref moves. `key` names anything else the result
/// depends on, such as the branch and the first week of a chart.
struct Snapshot<T> {
    refs: HashMap<String, String>,
    key: String,
    value: T,
}

type RefCache<T> = Arc<tokio::sync::Mutex<Option<Snapshot<T>>>>;
/// The repositories whose results are kept, least recently used first.
type RefCaches<T> = Mutex<VecDeque<(PathBuf, RefCache<T>)>>;

static CONTRIBUTOR_CACHES: LazyLock<RefCaches<Vec<ContributorInfo>>> =
    LazyLock::new(Mutex::default);
static CODE_FREQUENCY_CACHES: LazyLock<RefCaches<Vec<WeekChanges>>> = LazyLock::new(Mutex::default);

fn ref_cache<T>(caches: &RefCaches<T>, path: &Path) -> RefCache<T> {
    let mut caches = caches.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(at) = caches.iter().position(|(key, _)| key == path) {
        let entry = caches.remove(at).expect("cached history entry");
        let cache = entry.1.clone();
        caches.push_back(entry);
        return cache;
    }
    if caches.len() == 8 {
        if let Some(at) = caches
            .iter()
            .position(|(_, cache)| Arc::strong_count(cache) == 1)
        {
            caches.remove(at);
        }
    }
    let cache = Arc::new(tokio::sync::Mutex::new(None));
    if caches.len() < 8 {
        caches.push_back((path.to_owned(), cache.clone()));
    }
    cache
}

async fn current_refs(repo_path: &Path) -> Result<HashMap<String, String>> {
    let path = repo_path.to_owned();
    let permit = process::operation_slot(process::RPC_TIMEOUT).await?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        super::repository::list_mirror_refs(&path)
    })
    .await?
}

/// The cached value while the refs and `key` are the same, or `read`'s fresh one. A ref that
/// moved while `read` ran leaves the cache empty, so the next call reads again.
async fn cached_by_refs<T, F, Fut>(
    caches: &RefCaches<T>,
    repo_path: &Path,
    key: String,
    read: F,
) -> Result<T>
where
    T: Clone,
    F: FnOnce(HashMap<String, String>) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let cache = ref_cache(caches, repo_path);
    let mut cached = tokio::time::timeout(process::RPC_TIMEOUT, cache.lock()).await?;
    let refs = current_refs(repo_path).await?;
    if let Some(snapshot) = cached
        .as_ref()
        .filter(|snapshot| snapshot.refs == refs && snapshot.key == key)
    {
        return Ok(snapshot.value.clone());
    }
    let value = read(refs.clone()).await?;
    if refs == current_refs(repo_path).await? {
        *cached = Some(Snapshot {
            refs,
            key,
            value: value.clone(),
        });
    }
    Ok(value)
}

pub(crate) async fn get_contributors(repo_path: &Path) -> Result<Vec<ContributorInfo>> {
    cached_by_refs(&CONTRIBUTOR_CACHES, repo_path, String::new(), |_| async {
        let mut command = process::git_command()?;
        command
            .args(["shortlog", "-sne", "--all"])
            .current_dir(repo_path);
        let (status, output) = process::buffered(
            command,
            b"",
            MAX_CONTRIBUTOR_OUTPUT_BYTES,
            process::RPC_TIMEOUT,
        )
        .await?;
        ensure!(status.success(), "Git contributor listing failed");
        parse_contributors(&output)
    })
    .await
}

/// Lines added and removed on a branch in one week.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeekChanges {
    /// The Monday that starts the week, `YYYY-MM-DD`.
    pub week: String,
    pub additions: u64,
    pub deletions: u64,
}

/// A year of weeks, as GitHub's code frequency chart shows.
pub const CODE_FREQUENCY_WEEKS: usize = 52;
const MAX_CODE_FREQUENCY_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

fn week_start(date: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    date - chrono::Days::new(u64::from(date.weekday().num_days_from_monday()))
}

/// Sums `git log --format=@%ct --shortstat` into the weeks from `first` on.
fn parse_week_changes(output: &[u8], first: chrono::NaiveDate) -> Vec<WeekChanges> {
    let mut weeks: Vec<WeekChanges> = (0..CODE_FREQUENCY_WEEKS as u64)
        .map(|week| WeekChanges {
            week: (first + chrono::Days::new(week * 7)).to_string(),
            additions: 0,
            deletions: 0,
        })
        .collect();
    let mut current = None;
    for line in String::from_utf8_lossy(output).lines() {
        if let Some(time) = line.strip_prefix('@') {
            current = time
                .trim()
                .parse::<i64>()
                .ok()
                .and_then(|time| chrono::DateTime::from_timestamp(time, 0))
                .map(|time| (time.date_naive() - first).num_days())
                .filter(|days| *days >= 0)
                .map(|days| days as usize / 7)
                .filter(|week| *week < weeks.len());
            continue;
        }
        let Some(week) = current.and_then(|week| weeks.get_mut(week)) else {
            continue;
        };
        for part in line.split(',') {
            let mut words = part.split_whitespace();
            let (Some(count), Some(kind)) = (words.next(), words.next()) else {
                continue;
            };
            let Ok(count) = count.parse::<u64>() else {
                continue;
            };
            if kind.starts_with("insertion") {
                week.additions = week.additions.saturating_add(count);
            } else if kind.starts_with("deletion") {
                week.deletions = week.deletions.saturating_add(count);
            }
        }
    }
    weeks
}

/// Lines added and removed on `branch` in each of the last 52 weeks, oldest first. A branch
/// that does not exist yet has none.
pub(crate) async fn get_code_frequency(repo_path: &Path, branch: &str) -> Result<Vec<WeekChanges>> {
    let today = chrono::Utc::now().date_naive();
    let first = week_start(today) - chrono::Days::new((CODE_FREQUENCY_WEEKS as u64 - 1) * 7);
    let reference = format!("refs/heads/{branch}");
    let key = format!("{reference} {first}");
    cached_by_refs(&CODE_FREQUENCY_CACHES, repo_path, key, |refs| async move {
        if !refs.contains_key(&reference) {
            return Ok(parse_week_changes(b"", first));
        }
        let mut command = process::git_command()?;
        // A merge's lines were counted on the commits it brings in.
        command
            .args(["log", "--no-merges", "--format=@%ct", "--shortstat"])
            .arg(format!("--since={first} 00:00:00 +0000"))
            .arg(&reference)
            .arg("--")
            .current_dir(repo_path);
        let (status, output) = process::buffered(
            command,
            b"",
            MAX_CODE_FREQUENCY_OUTPUT_BYTES,
            process::RPC_TIMEOUT,
        )
        .await?;
        ensure!(status.success(), "Git code frequency listing failed");
        Ok(parse_week_changes(&output, first))
    })
    .await
}

/// An entry that another push removed while this scan ran (a quarantine directory, a lock
/// file) is not an error: it simply is not part of the size any more.
fn unless_removed<T>(result: std::io::Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn directory_size(path: &Path) -> Result<u64> {
    // The repository itself must exist; only what is inside it may vanish.
    std::fs::symlink_metadata(path)?;
    let mut size = 0u64;
    let mut pending = vec![path.to_owned()];
    let mut entries = 0usize;
    while let Some(path) = pending.pop() {
        let Some(listing) = unless_removed(std::fs::read_dir(&path))? else {
            continue;
        };
        for entry in listing {
            entries += 1;
            ensure!(
                entries <= 1_000_000 && pending.len() <= 8192,
                "repository metadata scan limit exceeded"
            );
            let Some(entry) = unless_removed(entry)? else {
                continue;
            };
            // Receive quarantine and pack temporaries are not durable repository storage.
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("incoming-") || name.starts_with("tmp_") || name.ends_with(".lock")
            {
                continue;
            }
            let Some(metadata) = unless_removed(std::fs::symlink_metadata(entry.path()))? else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                size = size.saturating_add(metadata.len());
            }
            // Never follow repository-owned symlinks during size accounting.
        }
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_frequency_sums_each_week_and_skips_older_commits() {
        let first = chrono::NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        let monday = first.and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp();
        let day = 86_400;
        let output = format!(
            "@{monday}\n\n 2 files changed, 10 insertions(+), 3 deletions(-)\n\
             @{}\n\n 1 file changed, 1 insertion(+)\n\
             @{}\n\n 1 file changed, 4 deletions(-)\n\
             @{}\n\n 1 file changed, 9 insertions(+)\n",
            monday + day,
            monday + 7 * day,
            monday - 3 * day,
        );
        let weeks = parse_week_changes(output.as_bytes(), first);
        assert_eq!(weeks.len(), CODE_FREQUENCY_WEEKS);
        assert_eq!(
            weeks[0],
            WeekChanges {
                week: "2026-01-05".into(),
                additions: 11,
                deletions: 3
            }
        );
        assert_eq!(weeks[1].week, "2026-01-12");
        assert_eq!((weeks[1].additions, weeks[1].deletions), (0, 4));
        assert!(weeks[2..]
            .iter()
            .all(|week| week.additions == 0 && week.deletions == 0));
        assert_eq!(week_start(first + chrono::Days::new(6)), first);
    }

    #[tokio::test]
    async fn code_frequency_reads_the_branch_and_a_missing_one_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(root.path()).unwrap();
        let empty = get_code_frequency(root.path(), "main").await.unwrap();
        assert_eq!(empty.len(), CODE_FREQUENCY_WEEKS);
        assert!(empty.iter().all(|week| week.additions == 0));
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let blob = repo.blob(b"one\ntwo\nthree\n").unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("notes.txt", blob, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "notes",
            &tree,
            &[],
        )
        .unwrap();
        let weeks = get_code_frequency(root.path(), "main").await.unwrap();
        let last = weeks.last().unwrap();
        assert_eq!((last.additions, last.deletions), (3, 0));
        assert!(get_code_frequency(root.path(), "other")
            .await
            .unwrap()
            .iter()
            .all(|week| week.additions == 0));
    }

    #[tokio::test]
    async fn contributors_are_cached_until_refs_change() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(root.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let first = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "one",
                &tree,
                &[],
            )
            .unwrap();
        assert_eq!(get_contributors(root.path()).await.unwrap()[0].commits, 1);
        // A cache hit need not read history again.
        let oid = first.to_string();
        let object = root.path().join("objects").join(&oid[..2]).join(&oid[2..]);
        let contents = std::fs::read(&object).unwrap();
        std::fs::remove_file(&object).unwrap();
        assert_eq!(get_contributors(root.path()).await.unwrap()[0].commits, 1);
        std::fs::write(&object, contents).unwrap();
        let parent = repo.find_commit(first).unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "two",
            &tree,
            &[&parent],
        )
        .unwrap();
        assert_eq!(get_contributors(root.path()).await.unwrap()[0].commits, 2);
    }

    #[tokio::test]
    async fn only_a_missing_branch_is_unborn_not_a_missing_object() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(root.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let tip = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "one",
                &tree,
                &[],
            )
            .unwrap()
            .to_string();
        assert_eq!(branch_tip(root.path(), "main").unwrap(), Some(tip.clone()));
        assert_eq!(branch_tip(root.path(), "absent").unwrap(), None);
        assert!(compute_language_stats(root.path(), "refs/heads/absent")
            .await
            .unwrap()
            .is_empty());
        std::fs::remove_file(root.path().join("objects").join(&tip[..2]).join(&tip[2..])).unwrap();
        assert!(branch_tip(root.path(), "main").is_err());
        assert!(compute_language_stats(root.path(), "refs/heads/main")
            .await
            .is_err());
    }

    #[test]
    fn a_file_removed_during_the_size_scan_is_skipped() {
        let missing = std::fs::metadata("/nonexistent/path/for/the/test");
        assert!(unless_removed(missing).unwrap().is_none());
        let denied = Err::<(), _>(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(unless_removed(denied).is_err());
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("pack"), [0u8; 10]).unwrap();
        assert_eq!(directory_size(directory.path()).unwrap(), 10);
        assert!(directory_size(&directory.path().join("gone")).is_err());
    }

    #[test]
    fn size_excludes_receive_quarantine_and_pack_temporaries() {
        let root = tempfile::tempdir().unwrap();
        for directory in ["objects/incoming-B", "objects/pack"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        for name in [
            "objects/incoming-B/pack",
            "objects/pack/tmp_pack_B",
            "HEAD.lock",
        ] {
            std::fs::write(root.path().join(name), [0u8; 100]).unwrap();
        }
        std::fs::write(
            root.path().join("objects/pack/pack-accepted.pack"),
            [0u8; 7],
        )
        .unwrap();
        assert_eq!(directory_size(root.path()).unwrap(), 7);
    }

    #[test]
    fn the_size_scan_survives_files_that_appear_and_vanish() {
        let directory = tempfile::tempdir().unwrap();
        let busy = directory.path().to_owned();
        let churn = std::thread::spawn(move || {
            for round in 0..3000 {
                let incoming = busy.join(format!("incoming-{round}"));
                std::fs::create_dir(&incoming).unwrap();
                std::fs::write(incoming.join("tmp_pack"), [0u8; 64]).unwrap();
                std::fs::remove_dir_all(&incoming).unwrap();
            }
        });
        while !churn.is_finished() {
            directory_size(directory.path()).unwrap();
        }
        churn.join().unwrap();
    }

    #[test]
    fn language_and_contributor_counts_accept_exactly_the_limit() {
        assert!(check_language_file_count(MAX_LANGUAGE_FILES).is_ok());
        assert!(check_language_file_count(MAX_LANGUAGE_FILES + 1)
            .unwrap_err()
            .is::<ObjectTooLarge>());
        assert!(check_contributor_count(MAX_CONTRIBUTORS).is_ok());
        assert!(check_contributor_count(MAX_CONTRIBUTORS + 1)
            .unwrap_err()
            .is::<ObjectTooLarge>());
    }

    #[test]
    fn language_paths_with_spaces_and_contributors_parse_without_extra_processes() {
        let languages = parse_language_stats(
            b"100644 blob 1111111111111111111111111111111111111111 12\tsrc/my file.rs\0",
        )
        .unwrap();
        assert_eq!(languages.len(), 1);
        assert_eq!(languages[0].language, "Rust");
        assert_eq!(languages[0].bytes, 12);

        let contributors = parse_contributors(b"  2\tA User <a@example.invalid>\n").unwrap();
        assert_eq!(contributors.len(), 1);
        assert_eq!(contributors[0].name, "A User");
        assert_eq!(contributors[0].commits, 2);
    }

    #[test]
    fn missing_size_is_a_failure_not_an_empty_repository() {
        let root = tempfile::tempdir().unwrap();
        assert!(directory_size(&root.path().join("missing")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn size_never_follows_repository_owned_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("object"), b"abc").unwrap();
        std::fs::write(outside.path().join("private"), b"must not be counted").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        std::os::unix::fs::symlink(root.path(), root.path().join("cycle")).unwrap();
        assert_eq!(directory_size(root.path()).unwrap(), 3);
    }
}
