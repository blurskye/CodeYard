//! Problem matchers keep multiline state for one job, as actions/runner's IssueMatcher does.
use super::commands::Masker;
pub(super) use crate::command_codec::{escape, escape_data};
use anyhow::{ensure, Context, Result};
use fancy_regex::{Regex, RegexBuilder};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const FIELDS: [&str; 7] = [
    "file", "line", "column", "severity", "code", "message", "fromPath",
];

#[derive(Default)]
pub(super) struct Matchers(Vec<Matcher>);

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "problemMatcher")]
    matchers: Vec<Definition>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Definition {
    owner: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    from_path: String,
    pattern: Vec<Pattern>,
}

#[derive(Deserialize)]
struct Pattern {
    regexp: String,
    #[serde(rename = "loop", default)]
    repeat: bool,
    #[serde(flatten)]
    groups: BTreeMap<String, usize>,
}

struct Matcher {
    owner: String,
    severity: String,
    from_path: String,
    patterns: Vec<(Regex, Pattern)>,
    state: Vec<Option<Issue>>,
}

#[derive(Clone, Default, Debug)]
pub(super) struct Issue(BTreeMap<String, String>);

fn severity(value: &str) -> Option<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "" | "error" => Some("error"),
        "warning" => Some("warning"),
        "notice" => Some("notice"),
        _ => None,
    }
}

impl Matcher {
    fn new(definition: Definition) -> Result<Self> {
        ensure!(
            !definition.owner.is_empty(),
            "Matcher owner must not be empty"
        );
        ensure!(
            severity(&definition.severity).is_some(),
            "Invalid matcher severity"
        );
        ensure!(
            !definition.pattern.is_empty() && definition.pattern.len() <= 64,
            "Invalid matcher pattern count"
        );
        let count = definition.pattern.len();
        let mut fields = BTreeSet::new();
        let mut patterns = Vec::new();
        for (index, pattern) in definition.pattern.into_iter().enumerate() {
            ensure!(
                !pattern.repeat
                    || (index > 0 && index + 1 == count && pattern.groups.contains_key("message")),
                "Only the last multiline pattern may loop and it must set message"
            );
            ensure!(
                pattern.regexp.len() <= 128 * 1024,
                "Matcher regexp is too large"
            );
            let regex = RegexBuilder::new(&pattern.regexp)
                .backtrack_limit(100_000)
                .build()
                .context("Invalid matcher regexp")?;
            for (field, group) in &pattern.groups {
                ensure!(FIELDS.contains(&field.as_str()), "Unknown matcher field");
                ensure!(fields.insert(field.clone()), "Matcher field is set twice");
                ensure!(
                    *group < regex.captures_len(),
                    "Matcher group is out of range"
                );
            }
            patterns.push((regex, pattern));
        }
        ensure!(fields.contains("message"), "Matcher must set message");
        Ok(Self {
            owner: definition.owner,
            severity: definition.severity,
            from_path: definition.from_path,
            patterns,
            state: vec![None; count.saturating_sub(1)],
        })
    }

    fn matches(&mut self, line: &str) -> Result<Option<Issue>> {
        for index in (0..self.patterns.len()).rev() {
            let running = if index == 0 {
                Some(Issue::default())
            } else {
                self.state[index - 1].clone()
            };
            let Some(mut issue) = running else {
                continue;
            };
            let (regex, pattern) = &self.patterns[index];
            if let Some(captures) = regex.captures(line)? {
                for (field, group) in &pattern.groups {
                    issue.0.entry(field.clone()).or_insert_with(|| {
                        captures
                            .get(*group)
                            .map_or("", |value| value.as_str())
                            .to_owned()
                    });
                }
                if index + 1 == self.patterns.len() {
                    let repeat = pattern.repeat;
                    let running = if index > 0 {
                        self.state[index - 1].clone()
                    } else {
                        None
                    };
                    self.state.fill(None);
                    if repeat {
                        self.state[index - 1] = running;
                    }
                    for (field, default) in
                        [("severity", &self.severity), ("fromPath", &self.from_path)]
                    {
                        if issue.0.get(field).is_none_or(String::is_empty) {
                            issue.0.insert(field.into(), default.clone());
                        }
                    }
                    return Ok(Some(issue));
                }
                self.state[index] = Some(issue);
            } else if index + 1 == self.patterns.len() {
                if index > 0 {
                    self.state[index - 1] = None;
                }
            } else {
                self.state[index] = None;
            }
        }
        Ok(None)
    }
}

impl Matchers {
    pub fn add(&mut self, path: &Path) -> Result<()> {
        // A FIFO without a writer would block a plain open, so open without waiting and check
        // the descriptor before reading.
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
        let file = options.open(path).context("Unable to read matcher file")?;
        ensure!(file.metadata()?.is_file(), "Matcher must be a regular file");
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1024 * 1024, "Matcher file is too large");
        let config: Config = serde_json::from_slice(&bytes).context("Invalid matcher JSON")?;
        let mut owners = BTreeSet::new();
        let mut added = Vec::new();
        for definition in config.matchers {
            ensure!(
                owners.insert(definition.owner.clone()),
                "Duplicate matcher owner"
            );
            added.push(Matcher::new(definition)?);
        }
        ensure!(
            self.0
                .iter()
                .filter(|matcher| !owners.contains(&matcher.owner))
                .count()
                + added.len()
                <= 64,
            "Too many problem matchers"
        );
        self.0.retain(|matcher| !owners.contains(&matcher.owner));
        self.0.extend(added);
        Ok(())
    }

    pub fn remove(&mut self, owner: &str) {
        self.0.retain(|matcher| matcher.owner != owner);
    }

    pub fn remove_file(&mut self, path: &Path) -> Result<()> {
        let mut removed = Self::default();
        removed.add(path)?;
        for matcher in removed.0 {
            self.remove(&matcher.owner);
        }
        Ok(())
    }

    pub fn reset(&mut self) {
        for matcher in &mut self.0 {
            matcher.state.fill(None);
        }
    }

    pub fn matches(&mut self, line: &str) -> Result<Option<Issue>> {
        // The runner ignores ANSI formatting when matching tool output.
        static ANSI: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let ansi = ANSI
            .get_or_init(|| regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("ANSI regexp"));
        let line = ansi.replace_all(line, "");
        for index in 0..self.0.len() {
            let matched = match self.0[index].matches(&line) {
                Ok(matched) => matched,
                Err(error) => {
                    self.0.remove(index);
                    return Err(error);
                }
            };
            if let Some(issue) = matched {
                for (other, matcher) in self.0.iter_mut().enumerate() {
                    if index != other {
                        matcher.state.fill(None);
                    }
                }
                return Ok(Some(issue));
            }
        }
        Ok(None)
    }
}

impl Issue {
    pub fn command(&self, workspace: &Path, masker: &Masker) -> Option<String> {
        let level = severity(self.0.get("severity").map_or("", String::as_str))?;
        let message = self
            .0
            .get("message")
            .filter(|message| !message.trim().is_empty())?;
        let mut properties = Vec::new();
        if let Some(file) = self.0.get("file").filter(|file| !file.is_empty()) {
            let from = self
                .0
                .get("fromPath")
                .map(Path::new)
                .and_then(Path::parent)
                .unwrap_or_else(|| Path::new(""));
            let path = workspace.join(from).join(file);
            let mut normalized = PathBuf::new();
            for part in path.components() {
                match part {
                    Component::ParentDir => {
                        normalized.pop();
                    }
                    Component::CurDir => {}
                    _ => normalized.push(part),
                }
            }
            if let Ok(relative) = normalized.strip_prefix(workspace) {
                properties.push(format!(
                    "file={}",
                    escape(&masker.mask(&relative.to_string_lossy()))
                ));
                for (field, property) in [("line", "line"), ("column", "col")] {
                    if let Some(number) = self
                        .0
                        .get(field)
                        .and_then(|value| value.parse::<u64>().ok())
                        .filter(|value| *value > 0)
                    {
                        properties.push(format!("{property}={number}"));
                    }
                }
            }
        }
        if let Some(code) = self.0.get("code").filter(|code| !code.is_empty()) {
            properties.push(format!("title={}", escape(&masker.mask(code))));
        }
        let properties = if properties.is_empty() {
            String::new()
        } else {
            format!(" {}", properties.join(","))
        };
        let message = escape_data(&masker.mask(message));
        Some(format!("::{level}{properties}::{message}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(value: serde_json::Value) -> Matcher {
        Matcher::new(serde_json::from_value(value).unwrap()).unwrap()
    }

    #[test]
    fn multiline_loops_retain_the_header_until_reset() {
        let mut matcher = matcher(serde_json::json!({"owner":"test", "pattern":[
            {"regexp":"^file: (.*)$", "file":1},
            {"regexp":"^(.*)$", "message":1, "loop":true}
        ]}));
        assert!(matcher.matches("file: a.rs").unwrap().is_none());
        assert_eq!(matcher.matches("one").unwrap().unwrap().0["file"], "a.rs");
        assert_eq!(matcher.matches("two").unwrap().unwrap().0["message"], "two");
        matcher.state.fill(None);
        assert!(matcher.matches("orphan").unwrap().is_none());
    }

    #[test]
    fn supports_lookahead_and_backreferences_with_a_bounded_engine() {
        let mut matcher = matcher(serde_json::json!({"owner":"test", "pattern":[
            {"regexp":r"^(?=error)(error) \1: (.*)$", "message":2}
        ]}));
        assert_eq!(
            matcher.matches("error error: detail").unwrap().unwrap().0["message"],
            "detail"
        );
    }

    #[test]
    fn validates_groups_loop_placement_and_duplicate_fields() {
        for patterns in [
            serde_json::json!([{ "regexp":"(x)", "message":2 }]),
            serde_json::json!([{ "regexp":"(x)", "message":1, "loop":true }]),
            serde_json::json!([{ "regexp":"(x)", "message":1 }, { "regexp":"(x)", "message":1 }]),
            serde_json::json!([{ "regexp":"(x)", "file":1 }]),
        ] {
            assert!(Matcher::new(
                serde_json::from_value(serde_json::json!({"owner":"test", "pattern": patterns}))
                    .unwrap()
            )
            .is_err());
        }
    }

    #[test]
    fn locations_are_relative_to_from_path_without_escaping_the_workspace() {
        let issue = Issue(BTreeMap::from([
            ("file".into(), "src/a.rs".into()),
            ("fromPath".into(), "sub/project.json".into()),
            ("line".into(), "7".into()),
            ("column".into(), "2".into()),
            ("message".into(), "bad\nvalue,100%".into()),
            ("severity".into(), "warning".into()),
        ]));
        assert_eq!(
            issue
                .command(Path::new("/workspace"), &Masker::default())
                .unwrap(),
            "::warning file=sub/src/a.rs,line=7,col=2::bad%0Avalue,100%25"
        );
        let mut outside = issue.clone();
        outside.0.insert("file".into(), "/outside/file".into());
        assert_eq!(
            outside
                .command(Path::new("/workspace"), &Masker::default())
                .unwrap(),
            "::warning::bad%0Avalue,100%25"
        );
        outside.0.insert("severity".into(), "invalid".into());
        assert!(outside
            .command(Path::new("/workspace"), &Masker::default())
            .is_none());
    }

    #[test]
    fn registration_replaces_by_owner_and_invalid_files_are_atomic() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("matcher.json");
        std::fs::write(
            &path,
            r#"{"problemMatcher":[{"owner":"test","pattern":[{"regexp":"(old)","message":1}]}]}"#,
        )
        .unwrap();
        let mut matchers = Matchers::default();
        matchers.add(&path).unwrap();
        assert!(matchers
            .matches("\u{1b}[31mold\u{1b}[0m")
            .unwrap()
            .is_some());
        std::fs::write(
            &path,
            r#"{"problemMatcher":[{"owner":"test","pattern":[{"regexp":"(new)","message":1}]}]}"#,
        )
        .unwrap();
        matchers.add(&path).unwrap();
        assert!(matchers.matches("old").unwrap().is_none());
        assert!(matchers.matches("new").unwrap().is_some());
        std::fs::write(&path, "{}").unwrap();
        assert!(matchers.add(&path).is_err());
        assert!(matchers.matches("new").unwrap().is_some());
        matchers.remove("test");
        assert!(matchers.matches("new").unwrap().is_none());
    }
}
