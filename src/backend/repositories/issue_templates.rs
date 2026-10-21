//! Issue and pull request templates kept in the repository. They are read from the default branch
//! through the repository store, so they work on local and remote storage alike.
//!
//! Issue templates are `.github/ISSUE_TEMPLATE/*.md`, or the single `.github/ISSUE_TEMPLATE.md`
//! when that directory has none. A `.yml` file in the directory is an issue form: fields to fill in
//! instead of a Markdown body. `config.yml` in the directory can switch blank issues off. The
//! pull request template is the first of a few fixed paths that exists.

use super::store::{ErrorKind, RepoError, RepoStore};

/// Larger template files are ignored.
pub const MAX_TEMPLATE_BYTES: u64 = 64 * 1024;
/// The chooser lists at most this many templates, in path order.
pub const MAX_TEMPLATES: usize = 20;

const ISSUE_TEMPLATE_DIRECTORY: &str = ".github/ISSUE_TEMPLATE";
const ISSUE_TEMPLATE_FILE: &str = ".github/ISSUE_TEMPLATE.md";
const PULL_REQUEST_TEMPLATE_PATHS: [&str; 4] = [
    ".github/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    "PULL_REQUEST_TEMPLATE.md",
    "docs/PULL_REQUEST_TEMPLATE.md",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueTemplate {
    /// The file's path in the repository. The chooser names a template by it.
    pub path: String,
    pub revision: String,
    pub name: String,
    pub about: String,
    pub title: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub body: String,
    /// The fields of an issue form. A Markdown template has none.
    pub form: Vec<FormElement>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FormKind {
    /// Text shown on the form. It is not part of the issue.
    Markdown,
    #[default]
    Input,
    Textarea,
    Dropdown,
    Checkboxes,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormOption {
    pub label: String,
    /// Only for checkboxes: the box has to be ticked.
    pub required: bool,
}

/// One element of an issue form, in the order the file lists them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormElement {
    pub kind: FormKind,
    pub label: String,
    pub description: String,
    pub placeholder: String,
    /// A text field's starting text, or the text of a Markdown element.
    pub value: String,
    pub options: Vec<FormOption>,
    /// Dropdown only: more than one option can be chosen.
    pub multiple: bool,
    pub required: bool,
    /// Textarea only: a language that fences the text as code in the issue.
    pub render: String,
    /// Dropdown only: the option chosen at the start.
    pub default: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueTemplates {
    pub templates: Vec<IssueTemplate>,
    /// False when `config.yml` sets `blank_issues_enabled: false`.
    pub blank_allowed: bool,
}

/// The issue templates of `branch`. An empty list means the repository has none.
pub async fn issue_templates(store: &RepoStore, branch: &str) -> Result<IssueTemplates, RepoError> {
    let Some(revision) = default_commit(store, branch).await? else {
        return Ok(IssueTemplates {
            templates: Vec::new(),
            blank_allowed: true,
        });
    };
    let mut paths = match store
        .tree_metadata(&revision, ISSUE_TEMPLATE_DIRECTORY)
        .await
    {
        Ok(entries) => template_paths(entries),
        Err(error) if is_missing(&error) => Vec::new(),
        Err(error) => return Err(error),
    };
    paths.push(format!("{ISSUE_TEMPLATE_DIRECTORY}/config.yml"));
    paths.push(ISSUE_TEMPLATE_FILE.to_owned());
    let mut texts = store.template_texts(&revision, &paths).await?;
    let fallback = texts.pop().flatten();
    let config = texts.pop().flatten();
    paths.truncate(paths.len() - 2);
    let blank_allowed = config.is_none_or(|(_, text)| blank_issues_allowed(&text));
    let mut files: Vec<_> = paths
        .into_iter()
        .zip(texts)
        .filter_map(|(path, text)| text.map(|(id, text)| (path, id, text)))
        .collect();
    if files.is_empty() {
        if let Some((id, text)) = fallback {
            files.push((ISSUE_TEMPLATE_FILE.to_owned(), id, text));
        }
    }
    let templates = tokio::task::spawn_blocking(move || {
        files
            .into_iter()
            .filter_map(|(path, revision, text)| {
                let mut template = if path.ends_with(".yml") || path.ends_with(".yaml") {
                    parse_form_template(&path, &text)?
                } else {
                    parse_template(&path, &text)
                };
                template.revision = revision;
                Some(template)
            })
            .collect()
    })
    .await
    .map_err(|_| {
        RepoError::new(
            ErrorKind::Unavailable,
            "Issue template parsing is unavailable",
        )
    })?;
    Ok(IssueTemplates {
        templates,
        blank_allowed,
    })
}

/// The pull request template of `branch`, if one of the known paths has text.
pub async fn pull_request_template(
    store: &RepoStore,
    branch: &str,
) -> Result<Option<String>, RepoError> {
    let Some(revision) = default_commit(store, branch).await? else {
        return Ok(None);
    };
    let paths: Vec<_> = PULL_REQUEST_TEMPLATE_PATHS
        .into_iter()
        .map(str::to_owned)
        .collect();
    let texts = store.template_texts(&revision, &paths).await?;
    Ok(texts.into_iter().flatten().next().map(|(_, text)| text))
}

async fn default_commit(store: &RepoStore, branch: &str) -> Result<Option<String>, RepoError> {
    match store.commit(&format!("refs/heads/{branch}")).await {
        Ok(commit) => Ok(Some(commit.sha)),
        Err(error) if is_missing(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// A `.md` or `.yml` file, not the `config.yml` of the directory.
fn is_template_name(name: &str) -> bool {
    name.ends_with(".md")
        || ((name.ends_with(".yml") || name.ends_with(".yaml")) && !name.starts_with("config."))
}

fn template_paths(entries: Vec<super::store::TreeEntry>) -> Vec<String> {
    let mut paths: Vec<String> = entries
        .into_iter()
        .filter(|entry| entry.entry_type == "file" && is_template_name(&entry.name))
        .filter(|entry| entry.size.is_some_and(|size| size <= MAX_TEMPLATE_BYTES))
        .map(|entry| entry.path)
        .collect();
    paths.sort();
    paths.truncate(MAX_TEMPLATES);
    paths
}

fn is_missing(error: &RepoError) -> bool {
    matches!(error.kind, ErrorKind::NotFound | ErrorKind::MissingPath)
}

/// `blank_issues_enabled: false` in the template directory's `config.yml`.
pub fn blank_issues_allowed(config: &str) -> bool {
    let setting = serde_yaml::from_str::<serde_yaml::Value>(config)
        .ok()
        .and_then(|root| root.get("blank_issues_enabled").cloned());
    !match setting {
        Some(serde_yaml::Value::Bool(enabled)) => !enabled,
        Some(serde_yaml::Value::String(text)) => text.trim() == "false",
        _ => false,
    }
}

fn parse_template(path: &str, text: &str) -> IssueTemplate {
    let (fields, body) = split_front_matter(text);
    let field = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| unquote(value).to_owned())
            .unwrap_or_default()
    };
    let list = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| list_items(value))
            .unwrap_or_default()
    };
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".md");
    let name = field("name");
    IssueTemplate {
        path: path.to_owned(),
        revision: String::new(),
        name: if name.is_empty() {
            stem.to_owned()
        } else {
            name
        },
        about: field("about"),
        title: field("title"),
        labels: list("labels"),
        assignees: list("assignees"),
        body: body.trim_start_matches(['\r', '\n']).to_owned(),
        form: Vec::new(),
    }
}

/// An issue form. A file that is not valid YAML, or has no fields to fill in, is left out, as
/// GitHub leaves it out of its chooser.
fn parse_form_template(path: &str, text: &str) -> Option<IssueTemplate> {
    let document = crate::backend::ci::workflows::parse_yaml_with_limit(
        text.as_bytes(),
        4 * MAX_TEMPLATE_BYTES as usize,
    )
    .ok()?;
    let text_of = |value: Option<&serde_yaml::Value>| match value {
        Some(serde_yaml::Value::String(text)) => text.trim_end_matches('\n').to_owned(),
        Some(serde_yaml::Value::Number(number)) => number.to_string(),
        Some(serde_yaml::Value::Bool(flag)) => flag.to_string(),
        _ => String::new(),
    };
    let names = |value: Option<&serde_yaml::Value>| match value {
        Some(serde_yaml::Value::Sequence(items)) => items
            .iter()
            .map(|item| text_of(Some(item)))
            .filter(|item| !item.is_empty())
            .collect(),
        Some(serde_yaml::Value::String(text)) => list_items(text),
        _ => Vec::new(),
    };
    let elements = document.get("body")?.as_sequence()?;
    if elements.len() > 50 {
        return None;
    }
    let form: Vec<FormElement> = elements
        .iter()
        .filter_map(|element| form_element(element, &text_of))
        .collect();
    if !form
        .iter()
        .any(|element| element.kind != FormKind::Markdown)
    {
        return None;
    }
    let stem = path.rsplit('/').next().unwrap_or(path);
    let stem = stem.trim_end_matches(".yaml").trim_end_matches(".yml");
    let name = text_of(document.get("name"));
    Some(IssueTemplate {
        path: path.to_owned(),
        revision: String::new(),
        name: if name.is_empty() {
            stem.to_owned()
        } else {
            name
        },
        about: text_of(document.get("description")),
        title: text_of(document.get("title")),
        labels: names(document.get("labels")),
        assignees: names(document.get("assignees")),
        body: String::new(),
        form,
    })
}

fn form_element(
    element: &serde_yaml::Value,
    text_of: &impl Fn(Option<&serde_yaml::Value>) -> String,
) -> Option<FormElement> {
    let kind = match element.get("type")?.as_str()? {
        "markdown" => FormKind::Markdown,
        "input" => FormKind::Input,
        "textarea" => FormKind::Textarea,
        "dropdown" => FormKind::Dropdown,
        "checkboxes" => FormKind::Checkboxes,
        _ => return None,
    };
    let attributes = element.get("attributes");
    let attribute = |key: &str| text_of(attributes.and_then(|map| map.get(key)));
    if attributes
        .and_then(|map| map.get("options"))
        .and_then(serde_yaml::Value::as_sequence)
        .is_some_and(|items| items.len() > 100)
    {
        return None;
    }
    let options = attributes
        .and_then(|map| map.get("options"))
        .and_then(serde_yaml::Value::as_sequence)
        .map(|items| {
            items
                .iter()
                .map(|item| match item.get("label") {
                    Some(label) => FormOption {
                        label: text_of(Some(label)),
                        required: item.get("required").and_then(serde_yaml::Value::as_bool)
                            == Some(true),
                    },
                    None => FormOption {
                        label: text_of(Some(item)),
                        required: false,
                    },
                })
                .filter(|option| !option.label.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let label = attribute("label");
    // Every field but the Markdown text needs a label to name it in the issue. A choice needs options.
    if kind != FormKind::Markdown && label.is_empty() {
        return None;
    }
    if matches!(kind, FormKind::Dropdown | FormKind::Checkboxes) && options.is_empty() {
        return None;
    }
    let default = attributes
        .and_then(|map| map.get("default"))
        .and_then(serde_yaml::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .filter(|index| *index < options.len())
        .unwrap_or(0);
    Some(FormElement {
        kind,
        label,
        description: attribute("description"),
        placeholder: attribute("placeholder"),
        value: attribute("value"),
        options,
        multiple: attributes
            .and_then(|map| map.get("multiple"))
            .and_then(serde_yaml::Value::as_bool)
            == Some(true),
        required: element
            .get("validations")
            .and_then(|map| map.get("required"))
            .and_then(serde_yaml::Value::as_bool)
            == Some(true),
        render: attribute("render"),
        default,
    })
}

/// The Markdown body of an issue made from a form, the way GitHub writes it: a `###` heading with
/// the label, then the answer. An empty answer reads `_No response_`. `answers` holds, for each
/// element in order, the chosen or typed values: text, the dropdown choices, or the ticked boxes.
pub fn compose_form_body(form: &[FormElement], answers: &[Vec<String>]) -> String {
    let mut sections = Vec::new();
    for (index, element) in form.iter().enumerate() {
        let given = answers.get(index).map(Vec::as_slice).unwrap_or(&[]);
        let answer = match element.kind {
            FormKind::Markdown => continue,
            FormKind::Input => given.first().cloned().unwrap_or_default(),
            FormKind::Textarea => {
                let text = given.first().cloned().unwrap_or_default();
                if text.trim().is_empty() || element.render.is_empty() {
                    text
                } else {
                    format!("```{}\n{}\n```", element.render, text.trim_end())
                }
            }
            FormKind::Dropdown => given.join(", "),
            FormKind::Checkboxes => element
                .options
                .iter()
                .map(|option| {
                    let mark = if given.contains(&option.label) {
                        'x'
                    } else {
                        ' '
                    };
                    format!("- [{mark}] {}", option.label)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let answer = if answer.trim().is_empty() {
            "_No response_".to_owned()
        } else {
            answer.trim().to_owned()
        };
        sections.push(format!("### {}\n\n{answer}", element.label));
    }
    sections.join("\n\n")
}

/// The first element the answers leave unfinished, by its label: a required field with no text,
/// a required checkbox left unticked, or a required group with nothing ticked.
pub fn first_missing_answer<'a>(
    form: &'a [FormElement],
    answers: &[Vec<String>],
) -> Option<&'a str> {
    form.iter().enumerate().find_map(|(index, element)| {
        let given = answers.get(index).map(Vec::as_slice).unwrap_or(&[]);
        let missing = match element.kind {
            FormKind::Markdown => false,
            FormKind::Input | FormKind::Textarea | FormKind::Dropdown => {
                element.required && given.iter().all(|text| text.trim().is_empty())
            }
            FormKind::Checkboxes => {
                (element.required
                    && !element
                        .options
                        .iter()
                        .any(|option| given.contains(&option.label)))
                    || element
                        .options
                        .iter()
                        .any(|option| option.required && !given.contains(&option.label))
            }
        };
        missing.then_some(element.label.as_str())
    })
}

/// Splits the `key: value` lines of a leading `---` block from the text after it. Text without
/// the block is all body.
fn split_front_matter(text: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (Vec::new(), text);
    };
    let mut fields = Vec::new();
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        if content == "---" {
            return (fields, &rest[offset + line.len()..]);
        }
        if let Some((key, value)) = content.split_once(':') {
            fields.push((key.trim().to_owned(), value.trim().to_owned()));
        }
        offset += line.len();
    }
    // No closing line: the block never ended, so the whole text is body.
    (Vec::new(), text)
}

fn unquote(value: &str) -> &str {
    value.trim().trim_matches(['"', '\''])
}

/// `a, b` or `[a, "b"]`, as a list of names.
fn list_items(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(unquote)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_reads_are_bounded_even_when_candidates_are_rejected() {
        let entries = (0..1000)
            .map(|i| super::super::store::TreeEntry {
                name: format!("{i:04}.md"),
                path: format!("{i:04}.md"),
                entry_type: "file".into(),
                size: Some(if i < 500 { MAX_TEMPLATE_BYTES + 1 } else { 10 }),
                last_commit: None,
            })
            .collect();
        let paths = template_paths(entries);
        assert_eq!(paths.len(), MAX_TEMPLATES);
        assert_eq!(paths.first().unwrap(), "0500.md");
        assert_eq!(paths.last().unwrap(), "0519.md");
    }

    #[test]
    fn front_matter_fields_and_body_split() {
        let text = "---\nname: Bug\nabout: \"Something breaks\"\nlabels: [bug, \"needs triage\"]\nassignees: alice, bob\n---\n\n## Steps\n";
        let template = parse_template(".github/ISSUE_TEMPLATE/bug.md", text);
        assert_eq!(template.name, "Bug");
        assert_eq!(template.about, "Something breaks");
        assert_eq!(template.labels, vec!["bug", "needs triage"]);
        assert_eq!(template.assignees, vec!["alice", "bob"]);
        assert_eq!(template.body, "## Steps\n");
        assert_eq!(template.title, "");
    }

    #[test]
    fn a_file_without_front_matter_is_all_body_and_named_by_its_file() {
        let template = parse_template(".github/ISSUE_TEMPLATE/feature.md", "Describe it\n");
        assert_eq!(template.name, "feature");
        assert_eq!(template.body, "Describe it\n");
        assert!(template.labels.is_empty());
    }

    #[test]
    fn an_unclosed_front_matter_block_is_body() {
        let text = "---\nname: Open\nno closing line\n";
        let template = parse_template("a.md", text);
        assert_eq!(template.name, "a");
        assert_eq!(template.body, text);
    }

    #[test]
    fn config_switches_blank_issues_off_only_when_false() {
        assert!(blank_issues_allowed(""));
        assert!(blank_issues_allowed("blank_issues_enabled: true\n"));
        assert!(!blank_issues_allowed("blank_issues_enabled: false\n"));
        assert!(!blank_issues_allowed(
            "blank_issues_enabled: false # use a template\n"
        ));
        assert!(!blank_issues_allowed(
            "contact_links: []\nblank_issues_enabled: 'false'\n"
        ));
    }

    const FORM: &str = "name: Bug report\ndescription: Something breaks\ntitle: \"[Bug] \"\nlabels: [bug, triage]\nbody:\n  - type: markdown\n    attributes:\n      value: Thanks for the report.\n  - type: input\n    attributes:\n      label: Version\n    validations:\n      required: true\n  - type: textarea\n    attributes:\n      label: Log\n      render: shell\n  - type: dropdown\n    attributes:\n      label: Area\n      options: [Web, Git]\n  - type: checkboxes\n    attributes:\n      label: Checks\n      options:\n        - label: I searched first\n          required: true\n        - label: I can help\n";

    #[test]
    fn issue_forms_bound_alias_expansion_fields_and_options() {
        let bomb = format!("name: &large {}\nbody:\n  - type: dropdown\n    attributes:\n      label: Choice\n      options: [{}]\n", "x".repeat(16_384), vec!["*large"; 5000].join(", "));
        assert!(bomb.len() < MAX_TEMPLATE_BYTES as usize);
        assert!(parse_form_template("bomb.yml", &bomb).is_none());
        let fields = format!(
            "body:\n{}",
            "  - type: input\n    attributes:\n      label: Field\n".repeat(51)
        );
        assert!(parse_form_template("fields.yml", &fields).is_none());
        let options = format!(
            "body:\n  - type: dropdown\n    attributes:\n      label: Choice\n      options: [{}]",
            vec!["x"; 101].join(", ")
        );
        assert!(parse_form_template("options.yml", &options).is_none());
    }

    #[test]
    fn an_issue_form_reads_its_fields() {
        let template = parse_form_template(".github/ISSUE_TEMPLATE/bug.yml", FORM).unwrap();
        assert_eq!(template.name, "Bug report");
        assert_eq!(template.about, "Something breaks");
        assert_eq!(template.title, "[Bug] ");
        assert_eq!(template.labels, vec!["bug", "triage"]);
        let kinds: Vec<FormKind> = template.form.iter().map(|element| element.kind).collect();
        assert_eq!(
            kinds,
            [
                FormKind::Markdown,
                FormKind::Input,
                FormKind::Textarea,
                FormKind::Dropdown,
                FormKind::Checkboxes
            ]
        );
        assert!(template.form[1].required);
        assert!(template.form[4].options[0].required);
    }

    #[test]
    fn a_form_without_fields_or_with_bad_yaml_is_left_out() {
        assert!(parse_form_template("a.yml", "name: Empty\nbody: []\n").is_none());
        assert!(parse_form_template("a.yml", "body: [unclosed\n").is_none());
        let only_text = "body:\n  - type: markdown\n    attributes:\n      value: hi\n";
        assert!(parse_form_template("a.yml", only_text).is_none());
    }

    #[test]
    fn a_form_answer_becomes_markdown_sections() {
        let form = parse_form_template("bug.yml", FORM).unwrap().form;
        let answers = vec![
            vec![],
            vec!["1.2".to_owned()],
            vec!["boom".to_owned()],
            vec!["Git".to_owned()],
            vec!["I searched first".to_owned()],
        ];
        assert_eq!(
            compose_form_body(&form, &answers),
            "### Version\n\n1.2\n\n### Log\n\n```shell\nboom\n```\n\n### Area\n\nGit\n\n### Checks\n\n- [x] I searched first\n- [ ] I can help"
        );
        assert!(compose_form_body(&form, &[]).contains("### Version\n\n_No response_"));
    }

    #[test]
    fn required_answers_are_checked() {
        let form = parse_form_template("bug.yml", FORM).unwrap().form;
        let mut answers = vec![
            vec![],
            vec![],
            vec![],
            vec!["Web".to_owned()],
            vec!["I searched first".to_owned()],
        ];
        assert_eq!(first_missing_answer(&form, &answers), Some("Version"));
        answers[1] = vec!["1".to_owned()];
        assert_eq!(first_missing_answer(&form, &answers), None);
        answers[4].clear();
        assert_eq!(first_missing_answer(&form, &answers), Some("Checks"));
    }

    #[test]
    fn only_issue_files_are_templates() {
        assert!(is_template_name("bug.md"));
        assert!(is_template_name("bug.yml"));
        assert!(!is_template_name("config.yml"));
        assert!(!is_template_name("notes.txt"));
    }
}
