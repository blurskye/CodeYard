use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ReleaseNotePull {
    pub number: i32,
    pub title: String,
    pub author: String,
    pub labels: Vec<String>,
}

native_operation!(crate::postgres::releases::notes::release_note_rows,
    crate::sqlite::releases::notes::release_note_rows,
    fn release_note_rows(pool:&DbPool,repo_id:i64,commits:&[String])->Result<Vec<(i32,String,String,Option<String>)>>);

/// Only pull requests whose recorded merge is in the pinned Git range contribute to the notes.
pub async fn release_note_pulls(
    pool: &DbPool,
    repo_id: i64,
    commits: &[String],
) -> Result<Vec<ReleaseNotePull>> {
    let mut pulls = BTreeMap::new();
    for (number, title, author, label) in release_note_rows(pool, repo_id, commits).await? {
        let pull = pulls.entry(number).or_insert_with(|| ReleaseNotePull {
            number,
            title,
            author,
            labels: Vec::new(),
        });
        if let Some(label) = label {
            pull.labels.push(label);
        }
    }
    anyhow::ensure!(
        pulls.len() <= 5000,
        "Too many pull requests for release notes"
    );
    Ok(pulls.into_values().collect())
}
