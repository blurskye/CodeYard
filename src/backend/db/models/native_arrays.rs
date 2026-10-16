//! Explicit native decoding for PostgreSQL arrays and SQLite JSON arrays.
use super::{BranchProtectionRule, ClusterNode, FileReasonLimit, JoinInvite};
use sqlx::{FromRow, Row};

impl<'r> FromRow<'r, sqlx::postgres::PgRow> for BranchProtectionRule {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            repo_id: row.try_get("repo_id")?,
            pattern: row.try_get("pattern")?,
            require_pull_request: row.try_get("require_pull_request")?,
            required_approving_reviews: row.try_get("required_approving_reviews")?,
            dismiss_stale_reviews: row.try_get("dismiss_stale_reviews")?,
            require_code_owner_reviews: row.try_get("require_code_owner_reviews")?,
            require_status_checks: row.try_get("require_status_checks")?,
            required_status_checks: row.try_get("required_status_checks")?,
            require_up_to_date_branch: row.try_get("require_up_to_date_branch")?,
            require_signed_commits: row.try_get("require_signed_commits")?,
            require_linear_history: row.try_get("require_linear_history")?,
            block_force_pushes: row.try_get("block_force_pushes")?,
            block_deletions: row.try_get("block_deletions")?,
            restrict_pushes: row.try_get("restrict_pushes")?,
            push_allowances: row.try_get("push_allowances")?,
            force_push_allowances: row.try_get("force_push_allowances")?,
            enforce_admins: row.try_get("enforce_admins")?,
            require_conversation_resolution: row.try_get("require_conversation_resolution")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

impl<'r> FromRow<'r, sqlx::sqlite::SqliteRow> for BranchProtectionRule {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            repo_id: row.try_get("repo_id")?,
            pattern: row.try_get("pattern")?,
            require_pull_request: row.try_get("require_pull_request")?,
            required_approving_reviews: row.try_get("required_approving_reviews")?,
            dismiss_stale_reviews: row.try_get("dismiss_stale_reviews")?,
            require_code_owner_reviews: row.try_get("require_code_owner_reviews")?,
            require_status_checks: row.try_get("require_status_checks")?,
            required_status_checks: serde_json::from_str(
                &row.try_get::<String, _>("required_status_checks")?,
            )
            .map_err(|error| sqlx::Error::ColumnDecode {
                index: "required_status_checks".into(),
                source: Box::new(error),
            })?,
            require_up_to_date_branch: row.try_get("require_up_to_date_branch")?,
            require_signed_commits: row.try_get("require_signed_commits")?,
            require_linear_history: row.try_get("require_linear_history")?,
            block_force_pushes: row.try_get("block_force_pushes")?,
            block_deletions: row.try_get("block_deletions")?,
            restrict_pushes: row.try_get("restrict_pushes")?,
            push_allowances: serde_json::from_str(&row.try_get::<String, _>("push_allowances")?)
                .map_err(|error| sqlx::Error::ColumnDecode {
                    index: "push_allowances".into(),
                    source: Box::new(error),
                })?,
            force_push_allowances: serde_json::from_str(
                &row.try_get::<String, _>("force_push_allowances")?,
            )
            .map_err(|error| sqlx::Error::ColumnDecode {
                index: "force_push_allowances".into(),
                source: Box::new(error),
            })?,
            enforce_admins: row.try_get("enforce_admins")?,
            require_conversation_resolution: row.try_get("require_conversation_resolution")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

impl<'r> FromRow<'r, sqlx::postgres::PgRow> for FileReasonLimit {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            reason: row.try_get("reason")?,
            max_file_size_bytes: row.try_get("max_file_size_bytes")?,
            max_uploads_per_hour: row.try_get("max_uploads_per_hour")?,
            max_bytes_per_hour: row.try_get("max_bytes_per_hour")?,
            max_uploads_per_day: row.try_get("max_uploads_per_day")?,
            max_bytes_per_day: row.try_get("max_bytes_per_day")?,
            allowed_content_types: row.try_get("allowed_content_types")?,
            requires_auth: row.try_get("requires_auth")?,
            description: row.try_get("description")?,
        })
    }
}

impl<'r> FromRow<'r, sqlx::sqlite::SqliteRow> for FileReasonLimit {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            reason: row.try_get("reason")?,
            max_file_size_bytes: row.try_get("max_file_size_bytes")?,
            max_uploads_per_hour: row.try_get("max_uploads_per_hour")?,
            max_bytes_per_hour: row.try_get("max_bytes_per_hour")?,
            max_uploads_per_day: row.try_get("max_uploads_per_day")?,
            max_bytes_per_day: row.try_get("max_bytes_per_day")?,
            allowed_content_types: row
                .try_get::<Option<String>, _>("allowed_content_types")?
                .map(|text| serde_json::from_str(&text))
                .transpose()
                .map_err(|error| sqlx::Error::ColumnDecode {
                    index: "allowed_content_types".into(),
                    source: Box::new(error),
                })?,
            requires_auth: row.try_get("requires_auth")?,
            description: row.try_get("description")?,
        })
    }
}

impl<'r> FromRow<'r, sqlx::postgres::PgRow> for JoinInvite {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            roles: row.try_get("roles")?,
            reusable: row.try_get("reusable")?,
            requires_approval: row.try_get("requires_approval")?,
            expires_at: row.try_get("expires_at")?,
            uses: row.try_get("uses")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

impl<'r> FromRow<'r, sqlx::sqlite::SqliteRow> for JoinInvite {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            roles: serde_json::from_str(&row.try_get::<String, _>("roles")?).map_err(|error| {
                sqlx::Error::ColumnDecode {
                    index: "roles".into(),
                    source: Box::new(error),
                }
            })?,
            reusable: row.try_get("reusable")?,
            requires_approval: row.try_get("requires_approval")?,
            expires_at: row.try_get("expires_at")?,
            uses: row.try_get("uses")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

impl<'r> FromRow<'r, sqlx::postgres::PgRow> for ClusterNode {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            public_key: row.try_get("public_key")?,
            name: row.try_get("name")?,
            roles: row.try_get("roles")?,
            observed_at: row.try_get("observed_at")?,
            status: row.try_get("status")?,
            addr_hint: row.try_get("addr_hint")?,
            version: row.try_get("version")?,
            disk_free: row.try_get("disk_free")?,
            last_seen: row.try_get("last_seen")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

impl<'r> FromRow<'r, sqlx::sqlite::SqliteRow> for ClusterNode {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            public_key: row.try_get("public_key")?,
            name: row.try_get("name")?,
            roles: serde_json::from_str(&row.try_get::<String, _>("roles")?).map_err(|error| {
                sqlx::Error::ColumnDecode {
                    index: "roles".into(),
                    source: Box::new(error),
                }
            })?,
            observed_at: row.try_get("observed_at")?,
            status: row.try_get("status")?,
            addr_hint: row.try_get("addr_hint")?,
            version: row.try_get("version")?,
            disk_free: row.try_get("disk_free")?,
            last_seen: row.try_get("last_seen")?,
            created_at: row.try_get("created_at")?,
        })
    }
}
