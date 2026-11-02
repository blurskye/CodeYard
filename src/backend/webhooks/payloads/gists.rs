use super::{time, user};
use crate::backend::{
    api::{
        github::{api_url, node_id},
        http::config::{http_clone_base, link_origin},
    },
    db::{
        gists::{Comment, Gist},
        models::User,
    },
    git::objects::TreeObjectEntry,
    repositories::store::{GistRevision, RepoError},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

pub fn gist(gist: &Gist, owner: &User, files: Value) -> Value {
    let url = api_url(&format!("/gists/{}", gist.id));
    let git = format!(
        "{}/gists/{}.git",
        http_clone_base().trim_end_matches('/'),
        gist.id
    );
    json!({"id":gist.id,"node_id":node_id::git("GIST",&gist.id),"url":url,"forks_url":format!("{url}/forks"),"commits_url":format!("{url}/commits"),"comments_url":format!("{url}/comments"),"html_url":format!("{}/gists/{}",link_origin(),gist.id),"git_pull_url":git,"git_push_url":git,"files":files,"public":gist.is_public,"description":gist.description,"comments":gist.comments,"comments_enabled":true,"owner":user(owner),"user":null,"created_at":time(gist.created_at),"updated_at":time(gist.updated_at),"truncated":false})
}
pub fn gist_file(id: &str, sha: &str, entry: &TreeObjectEntry, content: Option<&[u8]>) -> Value {
    let binary =
        content.is_some_and(|bytes| bytes.contains(&0) || std::str::from_utf8(bytes).is_err());
    let mime = crate::backend::api::repos::code::raw::raw_content_type(&entry.path, binary)
        .split(';')
        .next()
        .unwrap_or("application/octet-stream");
    let mut value = json!({"filename":entry.path,"type":mime,"raw_url":format!("{}/gists/{id}/raw/{sha}/{}",link_origin(),crate::webui::encode_component(&entry.path)),"size":entry.size.unwrap_or(0)});
    if let Some(language) = crate::backend::git::language_for_path(&entry.path) {
        value["language"] = json!(language);
    }
    if let Some(content) = content {
        value["truncated"] = json!(content.len() < entry.size.unwrap_or(0));
        value["encoding"] = json!(if binary { "base64" } else { "utf-8" });
        value["content"] = if binary {
            json!(STANDARD.encode(content))
        } else {
            json!(std::str::from_utf8(content).unwrap_or_default())
        };
    }
    value
}
pub fn gist_revision(
    id: &str,
    revision: &GistRevision,
    author: Option<&User>,
) -> Result<Value, RepoError> {
    let timestamp = crate::backend::git::gists::revision_time(revision.timestamp)?;
    Ok(
        json!({"url":api_url(&format!("/gists/{id}/{}",revision.sha)),"version":revision.sha,"user":author.map(user),"committed_at":time(timestamp),"change_status":{"additions":revision.additions,"deletions":revision.deletions,"total":revision.additions+revision.deletions}}),
    )
}
pub fn gist_comment(id: &str, owner: i64, comment: &Comment, author: Option<&User>) -> Value {
    json!({"id":comment.id,"node_id":node_id::encode("GC",comment.id),"url":api_url(&format!("/gists/{id}/comments/{}",comment.id)),"body":comment.body,"user":author.map(user),"author_association":if comment.user_id==Some(owner) {"OWNER"} else {"NONE"},"created_at":time(comment.created_at),"updated_at":time(comment.updated_at)})
}
