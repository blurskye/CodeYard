//! Release notes derived from a pinned Git range and the repository's real merged pull requests.
use crate::cluster::context::NodeContext;
use crate::repositories::releases::notes::generate_notes;
use crate::repositories::releases::notes::NotesInput;
use crate::repositories::releases::notes::ReleaseNotes;
use crate::repositories::releases::target_revision;
use crate::repositories::releases::unavailable;
use crate::repositories::releases::validate_fields;
use crate::repositories::releases::validate_tag;
use crate::repositories::releases::ReleaseError;
use crate::repositories::store::repo_store;
use crate::storage::range::ByteRange;
use codeyard_git::error::ErrorKind;
use codeyard_model::http_config::link_origin;
use serde::Deserialize;

pub struct ReleaseNotes {
    pub target: String,
    pub name: String,
    pub body: String,
}
impl ReleaseNotes {
    pub fn with_preface(&self, preface: Option<&str>) -> Result<String, ReleaseError> {
        let body = match preface.filter(|text| !text.is_empty()) {
            Some(text) => format!("{text}\n\n{}", self.body),
            None => self.body.clone(),
        };
        validate_fields(&ReleaseFields {
            body: Some(body.clone()),
            ..Default::default()
        })?;
        Ok(body)
    }
}
pub struct NotesInput<'a> {
    pub tag: &'a str,
    pub target: Option<&'a str>,
    pub previous_tag: Option<&'a str>,
    pub configuration_path: Option<&'a str>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    changelog: Changelog,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Changelog {
    #[serde(default)]
    exclude: Exclude,
    #[serde(default)]
    categories: Vec<Category>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Exclude {
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    authors: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Category {
    title: String,
    labels: Vec<String>,
    #[serde(default)]
    exclude: Exclude,
}
impl Exclude {
    fn excludes(&self, pull: &codeyard_db_repos::releases::notes::ReleaseNotePull) -> bool {
        self.authors
            .iter()
            .any(|author| author.eq_ignore_ascii_case(&pull.author))
            || self.labels.iter().any(|name| {
                pull.labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case(name))
            })
    }
}
fn markdown_text(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        if ch.is_control() {
            escaped.push(' ');
            continue;
        }
        if "\\`*_{}[]()<>#+-!|".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}
async fn configuration(
    store: &crate::repositories::store::RepoStore,
    sha: &str,
    path: Option<&str>,
) -> Result<Config, ReleaseError> {
    let paths = if let Some(path) = path {
        if path.is_empty()
            || path.len() > 1024
            || path.starts_with('/')
            || path.contains('\\')
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || path.chars().any(char::is_control)
        {
            return Err(ReleaseError::Invalid(
                "Invalid release notes configuration path",
            ));
        }
        vec![path]
    } else {
        vec![".github/release.yml", ".github/release.yaml"]
    };
    for candidate in paths {
        let blob = match store
            .blob_range(
                sha,
                candidate,
                BlobRange::Window {
                    offset: 0,
                    length: 65537,
                },
            )
            .await
        {
            Ok(blob) => blob,
            Err(error) if matches!(error.kind, ErrorKind::MissingPath | ErrorKind::NotFound) => {
                continue
            }
            Err(error) => return Err(ReleaseError::Git(error)),
        };
        if blob.size > 65536 || blob.is_binary || !matches!(blob.mode, 0o100644 | 0o100755) {
            return Err(ReleaseError::Invalid(
                "Release notes configuration must be a regular UTF-8 file of at most 64 KiB",
            ));
        }
        let yaml = codeyard_model::workflow_yaml::parse_yaml_with_limit(&blob.bytes, 8 * 65536)
            .map_err(|_| ReleaseError::Invalid("Invalid release notes YAML"))?;
        let config: Config = serde_yaml::from_value(yaml)
            .map_err(|_| ReleaseError::Invalid("Invalid release notes configuration"))?;
        if config.changelog.categories.len() > 100
            || config.changelog.categories.iter().any(|category| {
                category.title.is_empty()
                    || category.title.len() > 255
                    || category.title.chars().any(char::is_control)
                    || category.labels.len() > 200
                    || category.labels.is_empty()
            })
        {
            return Err(ReleaseError::Invalid("Invalid release notes categories"));
        }
        return Ok(config);
    }
    if path.is_some() {
        Err(ReleaseError::Invalid(
            "Release notes configuration does not exist",
        ))
    } else {
        Ok(Config::default())
    }
}

pub async fn generate_notes(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    actor: i64,
    repository_id: i64,
    input: &NotesInput<'_>,
) -> Result<ReleaseNotes, ReleaseError> {
    let (repository, _) = access::check_write(pool, repository_id, actor).await?;
    validate_tag(input.tag)?;
    if let Some(previous) = input.previous_tag {
        validate_tag(previous)?;
    }
    let store = repo_store(&repository, cluster);
    let refs = store.refs_from_primary().await.map_err(ReleaseError::Git)?;
    let revision = if refs.contains_key(&format!("refs/tags/{}", input.tag)) {
        format!("refs/tags/{}", input.tag)
    } else {
        match input.target {
            Some(target) => crate::repositories::releases::target_revision(&refs, target),
            None => format!("refs/heads/{}", repository.default_branch),
        }
    };
    if revision.is_empty() || revision.len() > 1024 {
        return Err(ReleaseError::TargetRequired);
    }
    let head = store
        .commit_from_primary(&revision)
        .await
        .map_err(ReleaseError::Git)?;
    if let Some(previous) = input.previous_tag {
        if !refs.contains_key(&format!("refs/tags/{previous}")) {
            return Err(ReleaseError::NotFound);
        }
    }
    let history = store
        .release_history(&head.sha, input.previous_tag, input.tag)
        .await
        .map_err(ReleaseError::Git)?;
    access::check_write(pool, repository_id, actor).await?;
    let config = configuration(&store, &head.sha, input.configuration_path).await?;
    access::check_write(pool, repository_id, actor).await?;
    let pulls = codeyard_db_repos::releases::notes::release_note_pulls(
        pool,
        repository_id,
        &history.commits,
    )
    .await
    .map_err(unavailable)?;
    let owner =
        codeyard_db_repos::repositories::records::repository_owner_name(pool, repository_id)
            .await
            .map_err(unavailable)?
            .ok_or(ReleaseError::NotFound)?;
    let encode = codeyard_webui::encode_component;
    let base = format!(
        "{}{}",
        link_origin(),
        codeyard_webui::repository_path(&owner, &repository.name)
    );
    let categories = if config.changelog.categories.is_empty() {
        vec![Category {
            title: "What's Changed".into(),
            labels: vec!["*".into()],
            exclude: Exclude::default(),
        }]
    } else {
        config.changelog.categories
    };
    let mut groups = vec![Vec::new(); categories.len()];
    for pull in &pulls {
        if config.changelog.exclude.excludes(pull) {
            continue;
        }
        if let Some(index) = categories.iter().position(|category| {
            !category.exclude.excludes(pull)
                && category.labels.iter().any(|name| {
                    name == "*"
                        || pull
                            .labels
                            .iter()
                            .any(|label| label.eq_ignore_ascii_case(name))
                })
        }) {
            groups[index].push(pull);
        }
    }
    let mut body = String::new();
    for (category, pulls) in categories.iter().zip(groups) {
        if pulls.is_empty() {
            continue;
        }
        let _ = writeln!(body, "## {}", markdown_text(&category.title));
        for pull in pulls {
            let _ = writeln!(
                body,
                "* {} by @{} in {base}/pulls/{}",
                markdown_text(&pull.title),
                pull.author,
                pull.number
            );
        }
        body.push('\n');
    }
    let comparison = if let Some(previous) = history.previous_tag {
        format!(
            "{base}/compare/{}...{}",
            encode(&format!("refs/tags/{previous}")),
            encode(&format!("refs/tags/{}", input.tag))
        )
    } else {
        format!(
            "{base}/commits/{}",
            encode(&format!("refs/tags/{}", input.tag))
        )
    };
    let _ = write!(body, "**Full Changelog**: {comparison}");
    validate_fields(&ReleaseFields {
        name: Some(input.tag.to_owned()),
        body: Some(body.clone()),
        ..Default::default()
    })?;
    // Slow Git/history reads cannot lend a revoked writer a private response.
    access::check_write(pool, repository_id, actor).await?;
    Ok(ReleaseNotes {
        target: head.sha,
        name: input.tag.to_owned(),
        body,
    })
}
