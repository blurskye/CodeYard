//! Gist lists, immutable file views and native editors, shared by SSR and hydration.
use serde::{Deserialize, Serialize};
mod page;
pub use page::render;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Page {
    List(Listing),
    Detail(Box<Detail>),
    Editor(Editor),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    pub owner: String,
    pub description: String,
    pub public: bool,
    pub filenames: Vec<String>,
    pub file_count: usize,
    /// The files could not be read; the card shows only the saved details.
    #[serde(default)]
    pub files_unavailable: bool,
    pub comments: i64,
    pub updated_at: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub cards: Vec<Card>,
    pub tab: String,
    pub owner: Option<String>,
    pub signed_in: bool,
    pub previous: Option<String>,
    pub next: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct File {
    pub filename: String,
    pub content: Option<String>,
    pub size: usize,
    pub truncated: bool,
    pub raw_url: String,
    pub image: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    pub sha: String,
    pub author: Option<String>,
    pub committed_at: String,
    pub additions: i32,
    pub deletions: i32,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: i64,
    pub author: Option<String>,
    pub body: String,
    pub created_at: String,
    pub can_edit: bool,
    pub can_delete: bool,
    pub edit_body: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detail {
    pub id: String,
    pub owner: String,
    pub description: String,
    pub public: bool,
    pub revision: String,
    pub historical: bool,
    pub parent: Option<String>,
    pub git_url: String,
    pub files: Vec<File>,
    pub revisions: Vec<Revision>,
    pub history_previous: Option<String>,
    pub history_next: Option<String>,
    pub forks: Vec<Card>,
    pub forks_previous: Option<String>,
    pub forks_next: Option<String>,
    pub comments: Vec<Comment>,
    pub comments_page: usize,
    pub comments_previous: Option<String>,
    pub comments_next: Option<String>,
    pub signed_in: bool,
    pub own: bool,
    pub starred: bool,
    pub csrf_token: String,
    pub message: Option<String>,
    pub comment_body: String,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EditorFile {
    pub original: Option<String>,
    pub filename: String,
    pub content: Option<String>,
    pub delete: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Editor {
    pub id: Option<String>,
    pub description: String,
    pub public: bool,
    pub expected: String,
    pub files: Vec<EditorFile>,
    pub csrf_token: String,
    pub message: Option<String>,
}
