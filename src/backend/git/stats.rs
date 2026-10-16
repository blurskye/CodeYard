use super::repository::{peel_to_commit_checked, process, ObjectTooLarge};
use anyhow::{ensure, Result};
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

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
        if let Some(language) = Path::new(filepath.as_ref())
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(extension_to_language)
        {
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

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<git2::Error>()
        .is_some_and(|error| error.code() == git2::ErrorCode::NotFound)
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

pub(crate) async fn get_contributors(repo_path: &Path) -> Result<Vec<ContributorInfo>> {
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
}

pub(crate) fn directory_size(path: &Path) -> Result<u64> {
    let mut size = 0u64;
    let mut pending = vec![path.to_owned()];
    let mut entries = 0usize;
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path)? {
            entries += 1;
            ensure!(
                entries <= 1_000_000 && pending.len() <= 8192,
                "repository metadata scan limit exceeded"
            );
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
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
