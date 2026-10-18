//! Repository topics and the website in the About panel. A save replaces the whole topic set in
//! one transaction, so a refused or failed save leaves the previous About text in place.
use crate::backend::db::repositories::records::RepositorySettingsRefusal;
use crate::backend::db::DbPool;
use anyhow::Result;
use std::collections::BTreeMap;

/// Most topics one repository carries.
pub const MAX_TOPICS: usize = 20;
/// Longest topic. Topics are ASCII, so bytes and characters agree.
pub const MAX_TOPIC_LEN: usize = 35;
/// Longest website address, the width of its column.
const MAX_WEBSITE_LEN: usize = 255;

/// Topics from a list of names. Each is lowercased, empty names are skipped and a repeated topic
/// is kept once, where it first appears.
pub fn parse_topics<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Vec<String>> {
    let mut topics: Vec<String> = Vec::new();
    for name in names {
        if name.is_empty() {
            continue;
        }
        let topic = name.to_ascii_lowercase();
        let valid = topic.len() <= MAX_TOPIC_LEN
            && topic
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !valid {
            return Err(RepositorySettingsRefusal::InvalidTopics.into());
        }
        if !topics.contains(&topic) {
            topics.push(topic);
        }
    }
    if topics.len() > MAX_TOPICS {
        return Err(RepositorySettingsRefusal::InvalidTopics.into());
    }
    Ok(topics)
}

/// The website of a repository, or none when the text is blank. Only http and https addresses
/// are kept, because the page links to the stored text.
pub fn parse_website(text: &str) -> Result<Option<String>> {
    let website = text.trim();
    if website.is_empty() {
        return Ok(None);
    }
    let web_address = url::Url::parse(website)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some());
    let plain = !website.chars().any(|c| c.is_whitespace() || c.is_control());
    if !web_address || !plain || website.len() > MAX_WEBSITE_LEN {
        return Err(RepositorySettingsRefusal::InvalidWebsite.into());
    }
    Ok(Some(website.to_owned()))
}

native_operation!(crate::backend::db::postgres::repositories::topics::update_about,
    crate::backend::db::sqlite::repositories::topics::update_about,
    /// Saves the description, website and topics together. Only a writer of an unarchived
    /// repository may save, and a refusal leaves all three unchanged.
    fn update_about(
    pool: &DbPool,
    actor_id: i64,
    repo_id: i64,
    description: &str,
    website: Option<&str>,
    topics: &[String],
) -> Result<()>);

native_operation!(crate::backend::db::postgres::repositories::topics::repository_topics,
    crate::backend::db::sqlite::repositories::topics::repository_topics,
    /// The topics of each listed repository, alphabetically. A repository without topics is
    /// absent from the map. This read grants no access; callers scope the repositories first.
    fn repository_topics(pool: &DbPool, repo_ids: &[i64]) -> Result<BTreeMap<i64, Vec<String>>>);
