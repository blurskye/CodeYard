//! Community standards are derived from real files at a pinned default-branch commit.

use chrono::DateTime;
use chrono::Utc;
use codeyard_database::models::Repository;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git::TreeEntry;
use codeyard_git_storage::context::NodeContext;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_git_storage::repositories::store::RepoStore;
use codeyard_storage::range::ByteRange as BlobRange;

#[derive(Clone)]
pub struct File {
    pub owner: String,
    pub repository: String,
    pub reference: String,
    pub path: String,
}
#[derive(Default)]
pub struct Profile {
    pub description: Option<String>,
    pub documentation: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub readme: Option<File>,
    pub license: Option<File>,
    pub contributing: Option<File>,
    pub conduct: Option<File>,
    pub issue_template: Option<File>,
    pub pull_request_template: Option<File>,
    pub issue_templates: Option<File>,
    pub pull_request_templates: Option<File>,
    pub license_info: Option<License>,
    pub contributor_covenant: bool,
}
impl Profile {
    pub fn health_percentage(&self) -> usize {
        let present = [
            self.description
                .as_ref()
                .is_some_and(|s| !s.trim().is_empty()),
            self.readme.is_some(),
            self.license.is_some(),
            self.contributing.is_some(),
            self.conduct.is_some(),
            self.issue_template.is_some() || self.issue_templates.is_some(),
            self.pull_request_template.is_some() || self.pull_request_templates.is_some(),
        ];
        present.into_iter().filter(|value| *value).count() * 100 / 7
    }
}

fn stem(name: &str) -> String {
    name.split('.')
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase()
        .replace(['-', '_'], "")
}
async fn text(store: &RepoStore, file: &File) -> Result<Option<String>, RepoError> {
    let blob = store
        .blob_range(
            &file.reference,
            &file.path,
            BlobRange::Window {
                offset: 0,
                length: 64 * 1024,
            },
        )
        .await?;
    Ok((!blob.is_binary && blob.size <= 64 * 1024)
        .then(|| String::from_utf8(blob.bytes).ok())
        .flatten())
}
fn file(owner: &str, repo: &Repository, reference: &str, entry: &TreeEntry) -> File {
    File {
        owner: owner.to_owned(),
        repository: repo.name.clone(),
        reference: reference.to_owned(),
        path: entry.path.clone(),
    }
}

/// The same pinned community-file search order is used by profiles and catalog search.
pub(super) async fn directories(
    store: &RepoStore,
    reference: &str,
) -> Result<Vec<(&'static str, Vec<TreeEntry>)>, RepoError> {
    let mut directories: Vec<(&str, Vec<TreeEntry>)> = Vec::new();
    for directory in ["", ".github", "docs"] {
        if !directory.is_empty()
            && !directories.first().is_some_and(|(_, entries)| {
                entries
                    .iter()
                    .any(|entry| entry.name == directory && entry.entry_type == "dir")
            })
        {
            continue;
        }
        match store.tree_metadata(reference, directory).await {
            Ok(entries) => directories.push((directory, entries)),
            Err(error) if error.kind == ErrorKind::MissingPath => {}
            Err(error) => return Err(error),
        }
    }
    Ok(directories)
}

async fn discover(
    repo: &Repository,
    owner: &str,
    cluster: Option<&NodeContext>,
) -> Result<Profile, RepoError> {
    let store = repo_store(repo, cluster);
    let refs = store.refs_from_primary().await?;
    let Some(reference) = refs.get(&format!("refs/heads/{}", repo.default_branch)) else {
        return Ok(Profile::default());
    };
    let mut profile = Profile::default();
    let directories = directories(&store, reference).await?;
    for (_, entries) in &directories {
        for entry in entries.iter().filter(|entry| entry.entry_type == "file") {
            match stem(&entry.name).as_str() {
                "readme" if profile.readme.is_none() => {
                    profile.readme = Some(file(owner, repo, reference, entry))
                }
                "license" | "licence" | "copying" | "unlicense" if profile.license.is_none() => {
                    profile.license = Some(file(owner, repo, reference, entry))
                }
                _ => {}
            }
        }
    }
    for directory in [".github", "", "docs"] {
        let Some((_, entries)) = directories.iter().find(|(path, _)| *path == directory) else {
            continue;
        };
        for entry in entries {
            let candidate = file(owner, repo, reference, entry);
            match (stem(&entry.name).as_str(), entry.entry_type.as_str()) {
                ("codeofconduct", "file") if profile.conduct.is_none() => {
                    profile.conduct = Some(candidate)
                }
                ("contributing", "file") if profile.contributing.is_none() => {
                    profile.contributing = Some(candidate)
                }
                ("issuetemplate", "file") if profile.issue_template.is_none() => {
                    profile.issue_template = Some(candidate)
                }
                ("pullrequesttemplate", "file") if profile.pull_request_template.is_none() => {
                    profile.pull_request_template = Some(candidate)
                }
                ("issuetemplate", "dir") if profile.issue_templates.is_none() => {
                    // Only a directory with a template the issue form offers counts; a lone
                    // config.yml offers none.
                    let entries = store.tree_metadata(reference, &entry.path).await?;
                    if !crate::repositories::issue_templates::template_paths(entries).is_empty() {
                        profile.issue_templates = Some(candidate)
                    }
                }
                ("pullrequesttemplate", "dir") if profile.pull_request_templates.is_none() => {
                    profile.pull_request_templates = Some(candidate)
                }
                _ => {}
            }
        }
    }
    if let Some(file) = &profile.license {
        profile.license_info = Some(license_info(
            text(&store, file).await?.as_deref().unwrap_or(""),
        ));
    }
    if let Some(file) = &profile.conduct {
        profile.contributor_covenant = text(&store, file)
            .await?
            .is_some_and(|text| contributor_covenant(&text));
    }
    let populated = profile.readme.is_some()
        || profile.license.is_some()
        || profile.conduct.is_some()
        || profile.contributing.is_some()
        || profile.issue_template.is_some()
        || profile.issue_templates.is_some()
        || profile.pull_request_template.is_some()
        || profile.pull_request_templates.is_some();
    if populated {
        profile.updated_at = Some(repo.updated_at);
    }
    Ok(profile)
}

pub async fn load(
    pool: &codeyard_database::DbPool,
    repo: &Repository,
    owner: &str,
    cluster: Option<&NodeContext>,
) -> Result<Profile, RepoError> {
    if repo.is_fork {
        return Err(RepoError::new(
            ErrorKind::MissingPath,
            "Community profiles are not available for forks",
        ));
    }
    let mut profile = discover(repo, owner, cluster).await?;
    profile.description = repo.description.clone();
    profile.documentation = repo.website.clone();
    let needs_defaults = profile.conduct.is_none()
        || profile.contributing.is_none()
        || (profile.issue_template.is_none() && profile.issue_templates.is_none())
        || (profile.pull_request_template.is_none() && profile.pull_request_templates.is_none());
    if repo.name != ".github" && needs_defaults {
        if let Some(default) =
            codeyard_db_repos::repositories::records::get_repository(pool, owner, ".github")
                .await
                .map_err(RepoError::from)?
        {
            if !default.is_private
                && codeyard_identity::repository_access::can_read(pool, None, &default)
                    .await
                    .map_err(RepoError::from)?
            {
                let inherited = match discover(&default, owner, cluster).await {
                    Ok(inherited) => inherited,
                    Err(_) => return Ok(profile), // Optional owner defaults cannot hide local files.
                };
                // Defaults are public inheritance, not a retained grant to a later private revision.
                if codeyard_identity::repository_access::check_read(pool, default.id, None)
                    .await
                    .is_err()
                {
                    return Ok(profile);
                }
                if profile.conduct.is_none() {
                    profile.conduct = inherited.conduct;
                    profile.contributor_covenant = inherited.contributor_covenant;
                }
                if profile.contributing.is_none() {
                    profile.contributing = inherited.contributing;
                }
                if profile.issue_template.is_none() && profile.issue_templates.is_none() {
                    profile.issue_template = inherited.issue_template;
                    profile.issue_templates = inherited.issue_templates;
                }
                if profile.pull_request_template.is_none()
                    && profile.pull_request_templates.is_none()
                {
                    profile.pull_request_template = inherited.pull_request_template;
                    profile.pull_request_templates = inherited.pull_request_templates;
                }
                if let Some(time) = inherited.updated_at {
                    profile.updated_at = Some(profile.updated_at.map_or(time, |old| old.max(time)));
                }
            }
        }
    }
    if profile.description.is_some() || profile.documentation.is_some() {
        profile.updated_at = Some(
            profile
                .updated_at
                .map_or(repo.updated_at, |old| old.max(repo.updated_at)),
        );
    }
    Ok(profile)
}

use codeyard_repos::community::{contributor_covenant, license_info, License};
