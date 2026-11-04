pub(crate) const MAX_CACHE_ENTRIES: usize = 1024;

/// The site, repository and organization cache settings: size and retention days for each.
pub(crate) type CacheSettings = (
    Option<i32>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
);

#[derive(sqlx::FromRow)]
pub struct RepositoryCacheUsage {
    pub name: String,
    pub count: i64,
    pub bytes: i64,
}

pub struct OrganizationCacheUsage {
    pub total: i64,
    pub count: i64,
    pub bytes: i64,
    pub repositories: Vec<RepositoryCacheUsage>,
}
