use serde::Serialize;
use std::collections::HashMap;
use uuid::Uuid;
fn avatar_url(avatar_file_id: Option<Uuid>) -> Option<String> {
    avatar_file_id.map(|id| format!("/api/files/{}/download", id))
}

#[derive(Serialize, Clone)]
pub struct PublicUserResponse {
    pub id: i64,
    pub username: String,
    pub avatar_url: Option<String>,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub location: Option<String>,
    pub website: Option<String>,
    pub is_admin: bool,
    pub created_at: String,
}

impl PublicUserResponse {
    pub fn from_display(author: &codeyard_db_identity::users::accounts::AuthorDisplay) -> Self {
        Self {
            id: author.id,
            username: author.username.clone(),
            avatar_url: avatar_url(author.avatar_file_id),
            display_name: author.display_name.clone(),
            bio: author.bio.clone(),
            location: author.location.clone(),
            website: author.website.clone(),
            is_admin: author.is_admin,
            created_at: author.created_at.to_rfc3339(),
        }
    }
}

impl From<codeyard_database::models::User> for PublicUserResponse {
    fn from(user: codeyard_database::models::User) -> Self {
        Self {
            id: user.id,
            username: user.username.clone(),
            avatar_url: avatar_url(user.avatar_file_id),
            display_name: user.display_name,
            bio: user.bio,
            location: user.location,
            website: user.website,
            is_admin: user.is_admin,
            created_at: user.created_at.to_rfc3339(),
        }
    }
}

pub type Fields = HashMap<String, String>;

pub fn field<'a>(form: &'a Fields, name: &str) -> &'a str {
    form.get(name).map(String::as_str).unwrap_or("")
}
