#[derive(Default, sqlx::FromRow)]
pub struct OrgCacheSettings {
    pub cache_size_gib: Option<i32>,
    pub cache_retention_days: Option<i32>,
}

#[derive(Clone, Copy)]
pub enum CacheLimit {
    Storage,
    Retention,
}

impl CacheLimit {
    pub fn column(self) -> &'static str {
        match self {
            Self::Storage => "cache_size_gib",
            Self::Retention => "cache_retention_days",
        }
    }
    pub fn effective(
        self,
        site: Option<i32>,
        organization: Option<i32>,
        repository: Option<i32>,
    ) -> i32 {
        site.unwrap_or_else(|| self.default_value())
            .min(organization.unwrap_or(i32::MAX))
            .min(repository.unwrap_or(i32::MAX))
    }
    pub fn default_value(self) -> i32 {
        match self {
            Self::Storage => 10,
            Self::Retention => 7,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CachePolicyError {
    #[error("Repository administration denied")]
    Denied,
    #[error("Cache limit must be positive and no greater than the site limit")]
    Limit,
}
