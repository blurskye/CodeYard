//! File and stdout commands following actions/runner's `Runner.Worker/FileCommandManager.cs` and `ActionCommandManager.cs`.
use crate::progress::{Event, Update};
use anyhow::{bail, Result};
use base64::Engine;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    Arc, Mutex,
};

#[derive(Clone, Default)]
pub struct Masker {
    masks: Arc<Mutex<MaskSet>>,
    /// Bytes of the masks that steps asked for. The job's own secrets do not count.
    requested: Arc<AtomicUsize>,
    /// Set when a requested mask did not fit, after which no output is shown.
    hidden: Arc<AtomicBool>,
}

#[derive(Clone, Default)]
struct MaskSet {
    values: BTreeSet<String>,
    bytes: usize,
    matcher: Option<aho_corasick::AhoCorasick>,
    build_bytes: usize,
}

/// The masks a workflow's steps add are held in memory, so they may total this much.
const MAX_MASK_BYTES: usize = 1 << 20;
const MAX_MASKS: usize = 16_384;
const MAX_EXPANDED_MASK_BYTES: usize = 16 << 20;

impl Masker {
    /// New jobs inherit credentials, but not each other's subsequent stdout commands.
    pub(super) fn fork(&self) -> Self {
        Self {
            masks: Arc::new(Mutex::new(self.masks.lock().unwrap_or_else(|error| error.into_inner()).clone())),
            requested: Arc::new(AtomicUsize::new(0)),
            hidden: Arc::new(AtomicBool::new(self.hidden.load(Ordering::Relaxed))),
        }
    }

    /// Adds a mask a step asked for, unless the steps' masks would pass their budget.
    pub fn add_requested(&self, value: &str) -> bool {
        let total = self
            .requested
            .fetch_add(value.len(), Ordering::Relaxed)
            .saturating_add(value.len());
        if total > MAX_MASK_BYTES {
            return false;
        }
        self.add(value);
        for word in value.split_whitespace() {
            self.add(word);
        }
        !self.hidden.load(Ordering::Relaxed)
    }

    /// Hides all later output. A value whose mask was refused could otherwise be printed.
    pub fn hide_all(&self) {
        self.hidden.store(true, Ordering::Relaxed);
    }

    pub fn add(&self, value: &str) {
        let mut masks = self.masks.lock().unwrap_or_else(|error| error.into_inner());
        for part in std::iter::once(value).chain(value.lines()).chain(value.lines().map(|line| line.trim_end_matches('\r'))) {
            if part.trim().is_empty() {
                continue;
            }
            let json = serde_json::to_string(part).unwrap_or_default();
            let json = json
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(part);
            let uri: String = part
                .bytes()
                .map(|byte| {
                    if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                        (byte as char).to_string()
                    } else {
                        format!("%{byte:02X}")
                    }
                })
                .collect();
            for encoded in [
                part.to_owned(),
                json.to_owned(),
                uri,
                base64::engine::general_purpose::STANDARD.encode(part),
            ] {
                if !masks.values.contains(&encoded) {
                    if masks.values.len() >= MAX_MASKS
                        || masks.bytes.saturating_add(encoded.len()) > MAX_EXPANDED_MASK_BYTES
                    {
                        self.hide_all();
                        return;
                    }
                    masks.bytes += encoded.len();
                    masks.values.insert(encoded);
                    masks.matcher = None;
                }
            }
        }
    }

    pub fn mask(&self, value: &str) -> String {
        if self.hidden.load(Ordering::Relaxed) {
            return "***".into();
        }
        let mut masks = self.masks.lock().unwrap_or_else(|error| error.into_inner());
        if masks.values.is_empty() { return value.to_owned(); }
        if masks.matcher.is_none() {
            masks.build_bytes = masks.build_bytes.saturating_add(masks.bytes);
            if masks.build_bytes > 64 * 1024 * 1024 { self.hide_all(); return "***".into(); }
            let matcher = aho_corasick::AhoCorasick::builder()
                .kind(Some(aho_corasick::AhoCorasickKind::ContiguousNFA)).build(masks.values.iter().map(String::as_bytes));
            let Ok(matcher) = matcher else { self.hide_all(); return "***".into(); };
            masks.matcher = Some(matcher);
        }
        // Redact the union of original-text matches, without sequential replacement.
        let mut boundaries = vec![0i32; value.len() + 1];
        for (index, found) in masks.matcher.as_ref().expect("compiled masks").find_overlapping_iter(value).enumerate() {
            if index >= 100_000 { return "***".into(); }
            boundaries[found.start()] += 1;
            boundaries[found.end()] -= 1;
        }
        let mut result = String::new();
        let mut active = 0;
        let mut copied = 0;
        for (index, delta) in boundaries.into_iter().enumerate() {
            let previous = active;
            active += delta;
            if previous == 0 && active > 0 {
                result.push_str(&value[copied..index]);
                result.push_str("***");
            } else if previous > 0 && active == 0 {
                copied = index;
            }
        }
        result.push_str(&value[copied..]);
        result
    }

    pub fn contains(&self, value: &str) -> bool {
        self.mask(value) != value
    }
}

#[derive(Clone)]
pub(super) struct Progress {
    pub nonce: Arc<str>,
    pub output: Option<super::output::Output>,
    pub job: String,
    pub variant: u32,
    pub step: Arc<AtomicU32>,
    pub active: Arc<Mutex<Option<(u32, std::time::Instant)>>>,
}
impl Progress {
    pub fn emit(&self, update: Update) {
        let at_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is before the epoch")
                .as_millis(),
        )
        .expect("system clock is out of range");
        let essential = !matches!(update, Update::Log { .. });
        let event = Event {
            job: self.job.clone(),
            variant: self.variant,
            at_ms,
            update,
        };
        if let Some(output) = &self.output {
            output.line(format!("::codeyard-progress::{}:{}", self.nonce,
                serde_json::to_string(&event).expect("runner progress is serializable")), essential);
        }
    }
}
/// The longest prefix of `text` within `max` bytes, cut on a character boundary.
pub(super) fn bounded(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[derive(Clone, Default)]
pub(super) struct Log {
    pub prefix: String,
    pub masker: Masker,
    pub progress: Option<Progress>,
    pub output: Option<super::output::Output>,
}

impl Log {
    pub fn emit(&self, update: Update) {
        if let Some(progress) = &self.progress {
            progress.emit(update);
        }
    }
    pub fn start_step(&self, number: u32, name: &str) {
        if let Some(progress) = &self.progress {
            progress.step.store(number, Ordering::Relaxed);
            *progress
                .active
                .lock()
                .unwrap_or_else(|error| error.into_inner()) =
                Some((number, std::time::Instant::now()));
            progress.emit(Update::StepStarted {
                number,
                name: bounded(&self.masker.mask(name), 1024),
            });
        }
    }
    pub fn finish_step(
        &self,
        number: u32,
        conclusion: crate::progress::Conclusion,
        started: std::time::Instant,
    ) {
        if let Some(progress) = &self.progress {
            progress
                .active
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            progress.emit(Update::StepFinished {
                number,
                conclusion,
                duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            });
            progress.step.store(0, Ordering::Relaxed);
        }
    }
    pub fn interrupt_step(&self, conclusion: crate::progress::Conclusion) {
        let active = self.progress.as_ref().and_then(|progress| {
            progress
                .active
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
        });
        if let Some((number, started)) = active {
            self.finish_step(number, conclusion, started);
        }
    }
    pub fn line(&self, text: &str) {
        let text = self.masker.mask(text);
        let prefix = self.masker.mask(&self.prefix);
        for line in text.split('\n') {
            if let Some(progress) = &self.progress {
                let number = progress.step.load(Ordering::Relaxed);
                let step = (number > 0).then_some(number);
                let mut rest = line;
                let chunk = if line.starts_with("Step started: ") || line.starts_with("Step completed: ") { 2048 } else { 512 };
                loop {
                    let mut end = rest.len().min(chunk);
                    while !rest.is_char_boundary(end) {
                        end -= 1;
                    }
                    progress.emit(Update::Log {
                        step,
                        text: format!("[{prefix}] {}", &rest[..end]),
                    });
                    rest = &rest[end..];
                    if rest.is_empty() {
                        break;
                    }
                }
            } else if let Some(output) = &self.output {
                output.line(format!("[{prefix}] {line}"), false);
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Commands {
    pub stopped: Option<String>,
    pub debug: bool,
    stderr: bool,
    retained_bytes: usize,
    retained_commands: usize,
    storage_full: bool,
    workspace: std::path::PathBuf,
    pub matchers: super::matchers::Matchers,
    pub failed: bool,
    pub outputs: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub paths: Vec<String>,
    pub state: BTreeMap<String, String>,
}

fn unescape(value: &str) -> String {
    value
        .replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%25", "%")
}

impl Commands {
    pub fn new(workspace: std::path::PathBuf, matchers: super::matchers::Matchers) -> Self {
        Self {
            workspace,
            matchers,
            ..Self::default()
        }
    }

    fn output(&mut self, log: &Log, line: &str, literal: bool) {
        if literal {
            log.line(&format!("| {line}"));
        } else {
            log.line(line);
        }
        match self.matchers.matches(line, self.stderr) {
            Ok(Some(issue)) => {
                if let Some(command) = issue.command(&self.workspace, &log.masker) {
                    log.line(&command);
                    self.command_line(log, &command, false, true);
                }
            }
            Ok(None) => {}
            Err(_) => log.line("Warning: Problem matcher exceeded its regexp limit"),
        }
    }

    #[cfg(test)]
    pub fn line(&mut self, log: &Log, line: &str, allow_unsafe: bool) {
        self.stream_line(log, line, allow_unsafe, false);
    }

    pub fn stream_line(&mut self, log: &Log, line: &str, allow_unsafe: bool, stderr: bool) {
        self.stderr = stderr;
        self.command_line(log, line, allow_unsafe, false);
    }

    /// `from_matcher` marks a command the runner generated for a problem matcher. Stop-commands
    /// does not hold those back, and they take the same annotation path as `::error::`.
    fn command_line(&mut self, log: &Log, line: &str, allow_unsafe: bool, from_matcher: bool) {
        let command = line
            .find("::")
            .and_then(|start| line[start + 2..].split_once("::"))
            .map(|(header, data)| (header, data, false))
            .or_else(|| {
                line.find("##[")
                    .and_then(|start| line[start + 3..].split_once(']'))
                    .map(|(header, data)| (header, data, true))
            });
        let Some((header, data, legacy)) = command else {
            self.output(log, line, self.stopped.is_some());
            return;
        };
        if let Some(token) = self.stopped.as_ref().filter(|_| !from_matcher) {
            if header == token {
                self.stopped = None;
            } else {
                self.output(log, line, true);
            }
            return;
        }
        let decode = |value: &str| {
            if legacy {
                unescape(&value.replace("%3B", ";").replace("%5D", "]"))
            } else {
                unescape(value)
            }
        };
        let (name, properties) = header.split_once(' ').unwrap_or((header, ""));
        let properties: BTreeMap<_, _> = properties
            .split(if legacy { ';' } else { ',' })
            .filter_map(|part| {
                let (key, value) = part.split_once('=')?;
                let value = if legacy {
                    decode(value)
                } else {
                    unescape(&value.replace("%3A", ":").replace("%2C", ","))
                };
                Some((key.to_ascii_lowercase(), value))
            })
            .collect();
        let data = decode(data);
        let name = name.to_ascii_lowercase();
        if matches!(name.as_str(), "set-output" | "save-state" | "set-env" | "add-path") {
            let bytes = data.len().saturating_add(properties.get("name").map_or(0, String::len));
            if self.storage_full || self.retained_commands >= 4096
                || bytes > (1usize << 20).saturating_sub(self.retained_bytes)
            {
                self.failed = true;
                self.storage_full = true;
                log.line("Error: Workflow commands exceed the runner storage limit");
                return;
            }
            self.retained_bytes += bytes;
            self.retained_commands += 1;
        }
        match name.as_str() {
            "add-mask" => {
                if !log.masker.add_requested(&data) {
                    self.failed = true;
                    log.line(
                        "Error: The job added more masks than the runner holds, so its later output is hidden",
                    );
                    log.masker.hide_all();
                }
            }
            "stop-commands" => {
                if data.is_empty()
                    || [
                        "add-mask",
                        "stop-commands",
                        "pause-logging",
                        "error",
                        "warning",
                        "notice",
                        "debug",
                        "group",
                        "endgroup",
                        "add-matcher",
                        "remove-matcher",
                    ]
                    .contains(&data.as_str())
                {
                    self.failed = true;
                    log.line("Error: Invalid stop-commands token");
                } else {
                    if data.len() > 6 && !log.masker.add_requested(&data) {
                        self.failed = true;
                        log.line(
                            "Error: The job added more masks than the runner holds, so its later output is hidden",
                        );
                        log.masker.hide_all();
                    }
                    self.stopped = Some(data);
                }
            }
            "set-env" | "add-path" if !allow_unsafe => {
                self.failed = true;
                log.line(&format!("Error: The `{name}` command is disabled. Please upgrade to using Environment Files."));
            }
            "set-env" => {
                if let Some(key) = properties.get("name") {
                    if !key.eq_ignore_ascii_case("NODE_OPTIONS") {
                        self.env.insert(key.clone(), data);
                    }
                }
            }
            "add-path" => self.paths.push(data),
            // The upstream runner still accepts set-output, with a deprecation warning.
            "set-output" => {
                log.line("Warning: The `set-output` command is deprecated. Please upgrade to using Environment Files.");
                if let Some(key) = properties.get("name") {
                    update_outputs(&mut self.outputs, [(key.clone(), data)]);
                }
            }
            "add-matcher" | "remove-matcher" => {
                let result = if name.eq_ignore_ascii_case("add-matcher") {
                    self.matchers.add(&self.workspace.join(&data))
                } else if let Some(owner) = properties.get("owner") {
                    self.matchers.remove(owner);
                    Ok(())
                } else {
                    self.matchers.remove_file(&self.workspace.join(&data))
                };
                if let Err(error) = result {
                    self.failed = true;
                    log.line(&format!("Error: {error:#}"));
                }
            }
            "error" | "warning" | "notice" => {
                let start_line = properties
                    .get("line")
                    .and_then(|line| line.parse::<u32>().ok())
                    .filter(|line| *line > 0)
                    .unwrap_or(1);
                let end_line = properties
                    .get("endline")
                    .and_then(|line| line.parse::<u32>().ok())
                    .filter(|line| *line >= start_line)
                    .unwrap_or(start_line);
                let start_column = (start_line == end_line).then(|| properties.get("col").and_then(|column| column.parse::<u32>().ok()).filter(|column| *column > 0)).flatten();
                let end_column = start_column.map(|start| properties.get("endcolumn").and_then(|column| column.parse::<u32>().ok()).filter(|column| *column >= start).unwrap_or(start));
                log.emit(Update::Annotation {
                    annotation: crate::progress::Annotation {
                        path: bounded(
                            &log.masker
                                .mask(properties.get("file").map_or("", String::as_str)),
                            256,
                        ),
                        start_line,
                        end_line,
                        start_column,
                        end_column,
                        annotation_level: match name.as_str() {
                            "error" => "failure",
                            "warning" => "warning",
                            _ => "notice",
                        }
                        .into(),
                        message: bounded(&log.masker.mask(&data), 1536),
                        title: properties
                            .get("title")
                            .map(|title| bounded(&log.masker.mask(title), 256)),
                    },
                });
                let properties = properties
                    .iter()
                    .map(|(key, value)| {
                        format!("{key}={}", super::matchers::escape(&log.masker.mask(value)))
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let properties = if properties.is_empty() {
                    String::new()
                } else {
                    format!(" {properties}")
                };
                // A problem matcher's command is already in the log, verbatim. Printing it again
                // would reorder its properties.
                if !from_matcher {
                    log.line(&format!(
                        "::{name}{properties}::{}",
                        super::matchers::escape_data(&log.masker.mask(&data))
                    ));
                }
            }
            "debug" if self.debug => log.line(&format!("debug: {data}")),
            "debug" => {},
            "group" => log.line(&format!(
                "::group::{}",
                super::matchers::escape_data(&log.masker.mask(&data))
            )),
            "endgroup" => log.line("::endgroup::"),
            "save-state" => {
                log.line("Warning: The `save-state` command is deprecated. Use GITHUB_STATE.");
                if let Some(key) = properties.get("name") {
                    self.state.insert(key.clone(), data);
                }
            }
            _ => self.output(log, line, false),
        }
    }
}

pub(super) fn update_outputs(target: &mut BTreeMap<String, String>, values: impl IntoIterator<Item = (String, String)>) {
    for (key, value) in values {
        target.retain(|name, _| !name.eq_ignore_ascii_case(&key));
        target.insert(key, value);
    }
}

pub(super) fn pairs(text: &str) -> Result<BTreeMap<String, String>> {
    Ok(entries(text)?.into_iter().collect())
}

pub(super) fn entries(text: &str) -> Result<Vec<(String, String)>> {
    let mut result = Vec::new();
    let mut lines = text.split_inclusive('\n');
    while let Some(raw) = lines.next() {
        let line = raw.strip_suffix('\n').unwrap_or(raw).trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let equals = line.find('=');
        let heredoc = line.find("<<");
        if let Some(index) = equals.filter(|index| heredoc.is_none_or(|h| *index < h)) {
            if index == 0 {
                bail!("Invalid format. Name must not be empty");
            }
            result.push((line[..index].into(), line[index + 1..].into()));
        } else if let Some(index) = heredoc {
            let key = &line[..index];
            let delimiter = &line[index + 2..];
            if key.is_empty() || delimiter.is_empty() {
                bail!("Invalid format. Name and delimiter must not be empty");
            }
            let mut value = String::new();
            loop {
                let next = lines.next().ok_or_else(|| {
                    anyhow::anyhow!("Invalid value. Matching delimiter not found '{delimiter}'")
                })?;
                if next.trim_end_matches('\n').trim_end_matches('\r') == delimiter {
                    break;
                }
                if !next.ends_with('\n') {
                    bail!("Invalid value. EOF marker missing new line.");
                }
                value.push_str(next);
            }
            if value.ends_with('\n') {
                value.pop();
                if value.ends_with('\r') {
                    value.pop();
                }
            }
            result.push((key.into(), value));
        } else {
            bail!("Invalid environment file format");
        }
    }
    Ok(result)
}

/// Split shell option strings without interpreting substitutions or executing a shell.
pub(super) fn words(text: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for ch in text.chars() {
        if escaped {
            if quote == Some('"') && !matches!(ch, '$' | '`' | '"' | '\\' | '\n') { word.push('\\'); }
            if ch != '\n' { word.push(ch); }
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && (ch == '\'' || ch == '"') {
            quote = Some(ch);
            started = true;
        } else if quote.is_none() && ch.is_whitespace() {
            if started {
                result.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(ch);
            started = true;
        }
    }
    if escaped || quote.is_some() {
        bail!("Unterminated quote in command options");
    }
    if started {
        result.push(word);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn poisoned_mask_storage_never_disables_redaction() {
        let masker = Masker::default();
        masker.add("private-credential");
        let other = masker.clone();
        assert!(std::thread::spawn(move || {
            let _guard = other.masks.lock().unwrap();
            panic!("simulated owner failure")
        })
        .join()
        .is_err());
        assert_eq!(masker.mask("private-credential"), "***");
        masker.add("another-credential");
        assert_eq!(masker.mask("another-credential"), "***");
    }
    #[test]
    fn environment_files() {
        let values = pairs("a=one=two\nb<<EOF\nline 1\nline 2\nEOF\nempty<<X\nX\n").unwrap();
        assert_eq!(values["a"], "one=two");
        assert_eq!(values["b"], "line 1\nline 2");
        assert_eq!(values["empty"], "");
        assert!(pairs("x<<EOF\nno end").is_err());
        assert!(pairs("broken").is_err());
    }
    #[test]
    fn masks_and_commands() {
        let masker = Masker::default();
        masker.add("sensitive-token\r");
        assert_eq!(masker.mask("sensitive-token"), "***");
        assert!(masker.add_requested("alpha-token beta-token"));
        assert_eq!(masker.mask("alpha-token"), "***");
        masker.add("abc123");
        masker.add("123456");
        assert_eq!(masker.mask("abc123456"), "***");
        for _ in 0..30 { masker.add("*"); }
        assert_eq!(masker.mask("***"), "***");
        masker.add("long secret\nshort");
        assert_eq!(masker.mask("long secret and short"), "*** and ***");
        let log = Log {
            prefix: "test".into(),
            masker,
            ..Default::default()
        };
        let mut commands = Commands::default();
        commands.line(&log, "::add-mask::new%25secret", false);
        assert_eq!(log.masker.mask("new%secret"), "***");
        commands.line(&log, "::stop-commands::token", false);
        commands.line(&log, "::set-output name=x::bad", false);
        assert!(commands.outputs.is_empty());
        commands.line(&log, "::token::", false);
        commands.line(&log, "::set-output name=x::good%0Avalue", false);
        assert_eq!(commands.outputs["x"], "good\nvalue");
        commands.line(&log, "::set-env name=x::bad", false);
        assert!(commands.failed);
    }
    #[test]
    fn legacy_matchers_obey_escaping_stopped_commands_and_unsafe_command_guards() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("matcher];.json"),
            r#"{"problemMatcher":[{"owner":"legacy;]","pattern":[{"regexp":"(diagnostic)","message":1}]}]}"#,
        )
        .unwrap();
        let log = Log {
            prefix: "test".into(),
            masker: Masker::default(),
            ..Default::default()
        };
        let mut commands = Commands::new(temp.path().into(), Default::default());
        commands.line(&log, "##[add-matcher]matcher%5D%3B.json", false);
        assert!(!commands.failed);
        assert!(commands.matchers.matches("diagnostic").unwrap().is_some());
        commands.line(&log, "::stop-commands::paused", false);
        commands.line(&log, "##[remove-matcher OWNER=legacy%3B%5D;]", false);
        assert!(commands.matchers.matches("diagnostic").unwrap().is_some());
        commands.line(&log, "::paused::", false);
        commands.line(&log, "##[remove-matcher OWNER=legacy%3B%5D;]", false);
        assert!(commands.matchers.matches("diagnostic").unwrap().is_none());
        commands.line(&log, "##[set-env name=X;]blocked", false);
        assert!(commands.failed);
        assert!(commands.env.is_empty());
    }

    #[test]
    fn legacy_save_state_is_kept_for_the_action() {
        let log = Log {
            prefix: "test".into(),
            masker: Masker::default(),
            ..Default::default()
        };
        let mut commands = Commands::default();
        commands.line(&log, "::save-state name=resource::abc", false);
        assert_eq!(commands.state["resource"], "abc");
    }

    #[test]
    fn splits_options() {
        assert_eq!(
            words("--label 'a=b c' --memory=2g \"\"").unwrap(),
            ["--label", "a=b c", "--memory=2g", ""]
        );
        assert!(words("'unfinished").is_err());
    }
}
