//! Ported from actions/runner's `src/Misc/expressionFunc/hashFiles/src/hashFiles.ts` and its bundled `@actions/glob`.
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use regex::Regex;
use sha2::{Digest, Sha256};

use super::Error;

struct Pattern {
    regex: Regex,
    exclude: bool,
    directory_only: bool,
    root: PathBuf,
    max_depth: Option<usize>,
}

fn path_text(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    if cfg!(windows) { text.replace('\\', "/") } else { text }
}

impl Pattern {
    fn parse(pattern: &str, workspace: &Path) -> Result<Self, Error> {
        let mut pattern = pattern;
        let mut exclude = false;
        while let Some(rest) = pattern.strip_prefix('!') {
            exclude = !exclude;
            pattern = rest.trim();
        }
        if pattern.is_empty() {
            return Err(Error::Evaluation("pattern cannot be empty".into()));
        }
        let parts: Vec<_> = pattern.split('/').filter(|p| !p.is_empty()).collect();
        if parts
            .iter()
            .enumerate()
            .any(|(i, p)| *p == ".." || (*p == "." && i != 0))
        {
            return Err(Error::Evaluation(format!(
                "Invalid pattern '{pattern}'. Relative pathing '.' and '..' is not allowed."
            )));
        }
        let mut root;
        let mut expression;
        let parts = if cfg!(windows) && pattern.as_bytes().get(1) == Some(&b':') {
            root = PathBuf::from(format!("{}/", parts[0]));
            expression = regex::escape(parts[0]);
            &parts[1..]
        } else if pattern.starts_with('/') {
            root = PathBuf::from("/");
            expression = String::new();
            &parts[..]
        } else if parts.first() == Some(&"~") {
            root = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| Error::Evaluation("Unable to determine HOME directory".into()))?;
            expression = regex::escape(path_text(&root).trim_end_matches('/'));
            &parts[1..]
        } else {
            root = workspace.to_path_buf();
            expression = regex::escape(path_text(workspace).trim_end_matches('/'));
            if parts.first() == Some(&".") {
                &parts[1..]
            } else {
                &parts[..]
            }
        };
        let base_depth = root.components().count();
        let max_depth = (!parts.contains(&"**")).then_some(base_depth + parts.len());
        let mut found_glob = false;
        for (i, part) in parts.iter().enumerate() {
            if !found_glob {
                if let Some(literal) = literal_segment(part) {
                    if literal == "." || literal == ".." || literal.contains('/') || (cfg!(windows) && (literal.contains('\\') || literal.contains(':'))) {
                        return Err(Error::Evaluation("Decoded glob segments must not change directories".into()));
                    }
                    root.push(literal);
                } else {
                    found_glob = true;
                }
            }
            if *part == "**" {
                if i + 1 == parts.len() {
                    expression.push_str("(?:/.*)?");
                } else {
                    expression.push_str("(?:/[^/]+)*");
                }
            } else {
                expression.push('/');
                expression.push_str(&segment_regex(part));
            }
        }
        if expression.is_empty() {
            expression.push('/');
        }
        let regex = regex::RegexBuilder::new(&format!("^(?:{expression})$"))
            .case_insensitive(cfg!(windows)).size_limit(256 * 1024).build()
            .map_err(|e| Error::Evaluation(format!("Invalid glob pattern '{pattern}': {e}")))?;
        Ok(Self {
            regex,
            exclude,
            root,
            directory_only: pattern.ends_with('/'),
            max_depth,
        })
    }

    fn matches(&self, path: &Path, directory: bool) -> bool {
        if (directory || !self.directory_only) && self.regex.is_match(&path_text(path)) {
            return true;
        }
        // @actions/glob adds an implicit /** after every pattern, including exclusions.
        path.ancestors()
            .skip(1)
            .any(|parent| self.regex.is_match(&path_text(parent)))
    }
}

fn literal_segment(segment: &str) -> Option<String> {
    let mut literal = String::new();
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => literal.push(chars.next().unwrap_or('\\')),
            '*' | '?' => return None,
            '[' => {
                let rest = chars.as_str();
                if let Some(end) = rest.find(']') {
                    let inside = &rest[..end];
                    if inside.chars().count() == 1 && !matches!(inside, "!" | "^") {
                        literal.push(inside.chars().next()?);
                        chars = rest[end + 1..].chars();
                    } else {
                        return None;
                    }
                } else {
                    literal.push(c);
                }
            }
            _ => literal.push(c),
        }
    }
    Some(literal)
}

fn segment_regex(segment: &str) -> String {
    let chars: Vec<_> = segment.chars().collect();
    let mut output = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => output.push_str("[^/]*"),
            '?' => output.push_str("[^/]"),
            '\\' if i + 1 < chars.len() => {
                i += 1;
                output.push_str(&regex::escape(&chars[i].to_string()));
            }
            '[' => {
                if let Some(end) = chars[i + 1..].iter().position(|c| *c == ']') {
                    let end = i + 1 + end;
                    let inside = &chars[i + 1..end];
                    let mut class = String::from("[");
                    for (n, c) in inside.iter().enumerate() {
                        if n == 0 && matches!(c, '!' | '^') {
                            class.push('^');
                            if inside.len() > 1 { class.push('/'); }
                        } else if matches!(c, '[' | '\\' | '&' | '~') {
                            class.push('\\');
                            class.push(*c);
                        } else {
                            class.push(*c);
                        }
                    }
                    class.push(']');
                    if Regex::new(&class).is_ok() {
                        output.push_str(&class);
                        i = end;
                    } else {
                        output.push_str("\\[");
                    }
                } else {
                    output.push_str("\\[");
                }
            }
            c => output.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    output
}

pub(super) fn hash_files(workspace: &str, arguments: &[String]) -> Result<String, Error> {
    let workspace = Path::new(workspace);
    if !workspace.is_absolute() {
        return Err(Error::Evaluation(
            "hashFiles requires an absolute github.workspace path".into(),
        ));
    }
    let workspace: PathBuf = workspace
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    let mut arguments = arguments;
    let mut follow_links = false;
    if let Some(first) = arguments.first().filter(|s| s.starts_with("--")) {
        if first.eq_ignore_ascii_case("--follow-symbolic-links") {
            follow_links = true;
            arguments = &arguments[1..];
        } else {
            return Err(Error::Evaluation(format!(
                "Invalid glob option {first}, available option: '--follow-symbolic-links'."
            )));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut patterns = Vec::new();
    let mut pattern_bytes = 0usize;
    for line in arguments.iter().flat_map(|argument| argument.lines()).map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        pattern_bytes = pattern_bytes.saturating_add(line.len());
        if patterns.len() >= 256 || pattern_bytes > 1_048_576 || super::execution_expired(deadline) {
            return Err(Error::Evaluation("hashFiles pattern budget exceeded".into()));
        }
        patterns.push(Pattern::parse(line, &workspace)?);
    }
    let candidates: BTreeSet<_> = patterns
        .iter()
        .filter(|p| !p.exclude)
        .map(|p| p.root.clone())
        .collect();
    let mut roots = Vec::new();
    for pattern in patterns.iter().filter(|p| !p.exclude) {
        if pattern
            .root
            .ancestors()
            .skip(1)
            .any(|p| candidates.contains(p))
        {
            continue;
        }
        // Never search outside the workspace. Ancestor roots can start at the workspace instead.
        let root = if pattern.root.starts_with(&workspace) {
            pattern.root.clone()
        } else if workspace.starts_with(&pattern.root) {
            workspace.clone()
        } else {
            continue;
        };
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    let mut visited = 0usize;
    let mut result = Sha256::new();
    let mut has_match = false;
    for root in roots {
        let mut stack = vec![(root, Vec::<PathBuf>::new())];
        while let Some((path, mut ancestors)) = stack.pop() {
            visited += 1;
            if visited > 100_000 || ancestors.len() > 256 {
                return Err(Error::Evaluation("hashFiles traversal budget exceeded".into()));
            }
            if super::execution_expired(deadline) {
                return Err(Error::Evaluation(
                    "hashFiles couldn't finish within 120 seconds".into(),
                ));
            }
            let metadata = if follow_links {
                fs::metadata(&path)
            } else {
                fs::symlink_metadata(&path)
            };
            let metadata = match metadata {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error(&path, error)),
            };
            if metadata.is_dir() {
                if !patterns.iter().filter(|pattern| !pattern.exclude).any(|pattern| {
                    pattern.matches(&path, true) || pattern.root.starts_with(&path)
                        || (path.starts_with(&pattern.root) && pattern.max_depth.is_none_or(|depth| path.components().count() < depth))
                }) { continue; }
                if follow_links {
                    let real = fs::canonicalize(&path).map_err(|e| io_error(&path, e))?;
                    if ancestors.contains(&real) {
                        continue;
                    }
                    ancestors.push(real);
                }
                // An excluded directory is skipped unless a later pattern can include again.
                let excluded = patterns
                    .iter()
                    .rposition(|pattern| pattern.exclude && pattern.matches(&path, true))
                    .is_some_and(|index| patterns[index + 1..].iter().all(|p| p.exclude));
                if excluded {
                    continue;
                }
                let mut children = Vec::new();
                for entry in fs::read_dir(&path).map_err(|e| io_error(&path, e))? {
                    if children.len() + stack.len() >= 100_000 || super::execution_expired(deadline) {
                        return Err(Error::Evaluation("hashFiles traversal budget exceeded".into()));
                    }
                    children.push(entry.map_err(|e| io_error(&path, e))?.path());
                }
                children.sort();
                stack.extend(
                    children
                        .into_iter()
                        .rev()
                        .map(|path| (path, ancestors.clone())),
                );
            } else {
                let mut matched = false;
                for pattern in &patterns {
                    if pattern.matches(&path, false) {
                        matched = !pattern.exclude;
                    }
                }
                if !matched {
                    continue;
                }
                // A file symlink is hashed even without --follow-symbolic-links; directory
                // symlinks are skipped in that mode, as in the runner's final stat call.
                let target = match fs::metadata(&path) {
                    Ok(target) => target,
                    // A dangling link names no file.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(io_error(&path, error)),
                };
                if !target.is_file() {
                    continue;
                }
                let mut options = File::options();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NONBLOCK);
                }
                let mut file = options.open(&path).map_err(|e| io_error(&path, e))?;
                if !file.metadata().map_err(|e| io_error(&path, e))?.is_file() { continue; }
                let mut hash = Sha256::new();
                let mut buffer = [0; 65536];
                loop {
                    if super::execution_expired(deadline) {
                        return Err(Error::Evaluation(
                            "hashFiles couldn't finish within 120 seconds".into(),
                        ));
                    }
                    let length = file.read(&mut buffer).map_err(|e| io_error(&path, e))?;
                    if length == 0 {
                        break;
                    }
                    hash.update(&buffer[..length]);
                }
                result.update(hash.finalize());
                has_match = true;
            }
        }
    }
    Ok(if has_match {
        format!("{:x}", result.finalize())
    } else {
        String::new()
    })
}

fn io_error(path: &Path, error: io::Error) -> Error {
    Error::Evaluation(format!(
        "hashFiles failed to hash '{}': {error}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(contents: &[&[u8]]) -> String {
        let mut hash = Sha256::new();
        for content in contents {
            hash.update(Sha256::digest(content));
        }
        format!("{:x}", hash.finalize())
    }
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        for (name, content) in [
            ("a.txt", "alpha"),
            ("b.txt", "beta"),
            (".hidden", "dot"),
            ("sub/c.txt", "gamma"),
            ("sub/package.json", "{}"),
            ("package.json", "[]"),
        ] {
            fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }
    fn run(dir: &Path, patterns: &[&str]) -> String {
        hash_files(
            dir.to_str().unwrap(),
            &patterns.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
        )
        .unwrap()
    }
    #[test]
    fn hashes_file_digests_in_glob_order() {
        let dir = fixture();
        let dir = dir.path();
        assert_eq!(
            run(dir, &["**/*.txt"]),
            digest(&[b"alpha", b"beta", b"gamma"])
        );
        assert_eq!(run(dir, &["b.txt", "a.txt"]), digest(&[b"beta", b"alpha"]));
        assert_eq!(
            run(dir, &["**/*.txt", "a.txt"]),
            digest(&[b"alpha", b"beta", b"gamma"])
        );
        assert_eq!(run(dir, &["a.txt"]), digest(&[b"alpha"]));
        assert_eq!(run(dir, &["nomatch"]), "");
        assert_eq!(run(dir, &["!**/*.txt"]), "");
        assert_eq!(run(dir, &["# comment\n\n"]), "");
        assert_eq!(run(dir, &["a.txt/"]), "");
    }
    #[test]
    fn exclusions_reinclusions_and_directories() {
        let dir = fixture();
        let dir = dir.path();
        assert_eq!(
            run(dir, &["**/*.txt", "!sub/**"]),
            digest(&[b"alpha", b"beta"])
        );
        assert_eq!(run(dir, &["**/*.txt", "!**", "a.txt"]), digest(&[b"alpha"]));
        assert_eq!(
            run(dir, &["**/*.txt\n!b.txt"]),
            digest(&[b"alpha", b"gamma"])
        );
        assert_eq!(run(dir, &["!!a.txt"]), digest(&[b"alpha"]));
        assert_eq!(run(dir, &["sub"]), digest(&[b"gamma", b"{}"]));
        assert_eq!(run(dir, &["sub/"]), digest(&[b"gamma", b"{}"]));
        assert_eq!(run(dir, &[".hidden"]), digest(&[b"dot"]));
        assert_eq!(run(dir, &["**/package.json"]), digest(&[b"[]", b"{}"]));
    }
    #[test]
    fn glob_characters_and_absolute_paths() {
        let dir = fixture();
        let dir = dir.path();
        assert_eq!(run(dir, &["[ab].txt"]), digest(&[b"alpha", b"beta"]));
        assert_eq!(run(dir, &["?.txt"]), digest(&[b"alpha", b"beta"]));
        assert_eq!(run(dir, &["[!b].txt"]), digest(&[b"alpha"]));
        assert_eq!(run(dir, &["./a.txt"]), digest(&[b"alpha"]));
        assert_eq!(
            run(dir, &[dir.join("a.txt").to_str().unwrap()]),
            digest(&[b"alpha"])
        );
        assert_eq!(run(dir, &["{a,b}.txt"]), "");
        assert!(hash_files(dir.to_str().unwrap(), &["../*.txt".into()]).is_err());
        assert!(hash_files(dir.to_str().unwrap(), &["--unknown".into()]).is_err());
        assert!(hash_files("relative", &["*".into()]).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_and_cycles() {
        use std::os::unix::fs::symlink;
        let dir = fixture();
        let dir = dir.path();
        symlink("a.txt", dir.join("link.txt")).unwrap();
        symlink("sub", dir.join("linked-sub")).unwrap();
        symlink("..", dir.join("sub/cycle")).unwrap();
        assert_eq!(run(dir, &["link.txt"]), digest(&[b"alpha"]));
        assert_eq!(run(dir, &["linked-sub/**"]), "");
        assert_eq!(
            run(dir, &["--follow-symbolic-links", "linked-sub/*.txt"]),
            digest(&[b"gamma"])
        );
        assert_eq!(
            run(dir, &["--follow-symbolic-links", "**/*.txt"]),
            digest(&[b"alpha", b"beta", b"alpha", b"gamma", b"gamma"])
        );
    }
    #[cfg(unix)]
    #[test]
    fn dangling_links_and_excluded_directories_are_skipped() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = fixture();
        let dir = dir.path();
        symlink("missing", dir.join("dangling.txt")).unwrap();
        assert_eq!(run(dir, &["*.txt"]), digest(&[b"alpha", b"beta"]));
        fs::set_permissions(dir.join("sub"), fs::Permissions::from_mode(0o000)).unwrap();
        let hashed = run(dir, &["**/*.txt", "!sub/**"]);
        fs::set_permissions(dir.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(hashed, digest(&[b"alpha", b"beta"]));
    }
}
