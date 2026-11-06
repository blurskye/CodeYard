//! Renders this cell's current search documents and writes them to the global index. Each
//! delivery reads the live rows, so a late or repeated delivery never sends an old state.
use crate::app::AppState;
use anyhow::{Context, Result};
use codeyard_db_core::{
    cells::search::{
        self, Batch, File, Index, Item, Version, MAXIMUM_CHUNK_BYTES, MAXIMUM_CODE_BYTES,
        MAXIMUM_DOCUMENT_BYTES, MAXIMUM_FILES, MAXIMUM_FILE_BYTES,
    },
    models::Repository,
    DbPool,
};
use codeyard_db_repos::assignees::AssignedItem;
use serde_json::{json, Value};

const MAXIMUM_BODY_CHARS: usize = 16_384;
const SYMLINK_MODE: u32 = 0o120000;

pub(super) async fn send(
    state: &AppState,
    index: &dyn Index,
    root: i64,
    item: Item,
    version: Version,
) -> Result<()> {
    let DbPool::Postgres(native) = &state.pool else {
        anyhow::bail!("Copy this SQLite cell to PostgreSQL before indexing it");
    };
    let (id, pull) = match item {
        Item::Repository => return repository(state, native, index, root, version).await,
        Item::Issue { id } => (id, false),
        Item::Pull { id } => (id, true),
    };
    // An item that moved is written under the repository that holds it now.
    let holder = search::item_repository(native, item).await?;
    let mut document = None;
    if let Some(repo) = match holder {
        Some(holder) => native_repository(state, holder).await?,
        None => None,
    } {
        let owner = owner(state, &repo).await?;
        let wanted = if pull {
            AssignedItem::PullRequest(id)
        } else {
            AssignedItem::Issue(id)
        };
        document = codeyard_webhooks::issue_documents::documents(
            &state.pool,
            codeyard_webhooks::issue_documents::Reader::Repository,
            &repo,
            &owner,
            &[wanted],
        )
        .await
        .map_err(|_| anyhow::anyhow!("Search document is unavailable"))?
        .pop()
        .map(fit);
    }
    let batch = Batch::Issue {
        id,
        pull,
        repository_id: holder.unwrap_or(root),
        version,
        document,
    };
    index.index(&batch).await?;
    Ok(())
}

/// The repository while it is a real repository homed in this cell.
async fn native_repository(state: &AppState, id: i64) -> Result<Option<Repository>> {
    let cell = i32::from(state.pool.cell_id().get());
    Ok(
        codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, id)
            .await?
            .filter(|repo| !repo.is_stub && !repo.is_gist && repo.home_cell == cell),
    )
}

async fn owner(state: &AppState, repo: &Repository) -> Result<String> {
    codeyard_db_repos::repositories::records::repository_owner_name(&state.pool, repo.id)
        .await?
        .context("Repository owner is missing")
}

/// Keeps a document under the index's size limit by shortening, then dropping, its body.
fn fit(mut document: Value) -> Value {
    let size = |document: &Value| serde_json::to_vec(document).map_or(usize::MAX, |v| v.len());
    if size(&document) > MAXIMUM_DOCUMENT_BYTES {
        if let Some(body) = document.get("body").and_then(Value::as_str) {
            document["body"] = json!(body.chars().take(MAXIMUM_BODY_CHARS).collect::<String>());
        }
    }
    if size(&document) > MAXIMUM_DOCUMENT_BYTES {
        document["body"] = Value::Null;
    }
    document
}

async fn repository(
    state: &AppState,
    native: &codeyard_db_core::pool::PgPool,
    index: &dyn Index,
    id: i64,
    version: Version,
) -> Result<()> {
    let Some(repo) = native_repository(state, id).await? else {
        let gone = codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, id)
            .await?
            .is_none();
        if gone {
            index
                .index(&Batch::Repository {
                    id,
                    version,
                    document: None,
                    public: false,
                    classic_allowed: true,
                    readers: Vec::new(),
                })
                .await?;
        }
        // A repository that moved away is written by its new home.
        return Ok(());
    };
    let owner = owner(state, &repo).await?;
    let public = !repo.is_private;
    let mut document = codeyard_webhooks::payloads::repository(&repo, &owner);
    let facts =
        codeyard_db_repos::repository_stats::repository_document_facts(&state.pool, id).await?;
    let topics = codeyard_db_repos::repositories::topics::repository_topics(&state.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    let forks = codeyard_db_repos::repositories::discovery::public_fork_counts(&state.pool, &[id])
        .await?
        .get(&id)
        .copied()
        .unwrap_or(0);
    document["private"] = json!(!public);
    document["language"] = json!(facts.language);
    document["pushed_at"] = json!(facts.pushed_at.map(|time| time.to_rfc3339()));
    document["topics"] = json!(topics);
    document["forks"] = json!(forks);
    document["forks_count"] = json!(forks);
    let access = search::access(native, id).await?;
    let indexed = index
        .index(&Batch::Repository {
            id,
            version,
            document: Some(fit(document)),
            public,
            classic_allowed: access.classic_allowed,
            readers: access.readers,
        })
        .await?;
    let store = codeyard_repos_core::repositories::store::repo_store(&repo, state.cluster.as_ref());
    let (refs, default) = store.refs_and_default().await?;
    let Some(head) = refs.get(&format!("refs/heads/{default}")).cloned() else {
        return Ok(());
    };
    if indexed.code_head.as_deref() == Some(head.as_str()) {
        return Ok(());
    }
    let paths = store.file_paths(&head, MAXIMUM_FILES).await?.paths;
    let mut chunk = Vec::new();
    let (mut chunk_bytes, mut total, mut first) = (0, 0, true);
    for path in paths {
        let blob = match store
            .blob_range(
                &head,
                &path,
                codeyard_storage::range::ByteRange::Window {
                    offset: 0,
                    length: MAXIMUM_FILE_BYTES as u64,
                },
            )
            .await
        {
            Ok(blob) => blob,
            Err(error) => {
                tracing::debug!(repository = id, %path, error = %error, "File is left out of the search index");
                continue;
            }
        };
        if blob.is_binary || blob.mode == SYMLINK_MODE || blob.size > MAXIMUM_FILE_BYTES as u64 {
            continue;
        }
        let Ok(content) = String::from_utf8(blob.bytes) else {
            continue;
        };
        if content.contains('\0') {
            continue;
        }
        let file = File {
            path,
            sha: blob.id,
            size: blob.size as i64,
            content,
        };
        let bytes = serde_json::to_vec(&file)?.len();
        if bytes > MAXIMUM_CHUNK_BYTES {
            continue;
        }
        total += file.path.len() + file.content.len();
        if total > MAXIMUM_CODE_BYTES {
            break;
        }
        if chunk_bytes + bytes > MAXIMUM_CHUNK_BYTES {
            send_code(
                index,
                id,
                version,
                &head,
                std::mem::take(&mut chunk),
                first,
                false,
            )
            .await?;
            first = false;
            chunk_bytes = 0;
        }
        chunk_bytes += bytes;
        chunk.push(file);
    }
    send_code(index, id, version, &head, chunk, first, true).await
}

async fn send_code(
    index: &dyn Index,
    repository_id: i64,
    version: Version,
    head: &str,
    files: Vec<File>,
    first: bool,
    last: bool,
) -> Result<()> {
    index
        .index(&Batch::Code {
            repository_id,
            version,
            head: head.to_owned(),
            files,
            first,
            last,
        })
        .await?;
    Ok(())
}
