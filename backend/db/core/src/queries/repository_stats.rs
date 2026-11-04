#[derive(Debug, sqlx::FromRow)]
pub struct RepositorySnapshot {
    #[sqlx(flatten)]
    pub repository: crate::models::Repository,
    #[sqlx(flatten)]
    pub facts: RepositoryDocumentFacts,
    pub owner: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct RepositoryDocumentFacts {
    pub open_items: i64,
    pub pushed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub language: Option<String>,
}
