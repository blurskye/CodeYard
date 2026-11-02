//! Team identities used by branch push restrictions. Repository authority is checked by callers;
//! these reads never grant access, and membership is evaluated again for each publication.
use super::protection::ProtectionWriteRefusal;
use crate::backend::db::{models::Team, DbPool};
use anyhow::Result;

pub(crate) fn validate_count(len: usize) -> Result<()> {
    anyhow::ensure!(
        len <= super::protection::MAX_ALLOWANCE_ACTORS,
        ProtectionWriteRefusal::InvalidPushTeams
    );
    Ok(())
}

native_operation!(crate::backend::db::postgres::repositories::push_teams::by_names,
    crate::backend::db::sqlite::repositories::push_teams::by_names,
    fn by_names(pool: &DbPool, repo: i64, names: &[String]) -> Result<Vec<Team>>);
native_operation!(crate::backend::db::postgres::repositories::push_teams::by_ids,
    crate::backend::db::sqlite::repositories::push_teams::by_ids,
    fn by_ids(pool: &DbPool, repo: i64, ids: &[i64]) -> Result<Vec<TeamAccess>>);
native_operation!(crate::backend::db::postgres::repositories::push_teams::contains_actor,
    crate::backend::db::sqlite::repositories::push_teams::contains_actor,
    fn contains_actor(pool: &DbPool, repo: i64, actor: i64, ids: &[i64]) -> Result<bool>);

pub async fn resolve_names(pool: &DbPool, repo: i64, names: &[String]) -> Result<Vec<i64>> {
    validate_count(names.len())?;
    let mut names = names.to_vec();
    anyhow::ensure!(
        names
            .iter()
            .all(|name| crate::backend::identity::credentials::validate_namespace(name, 1).is_ok()),
        ProtectionWriteRefusal::InvalidPushTeams
    );
    names.sort_unstable();
    names.dedup();
    let teams = by_names(pool, repo, &names).await?;
    anyhow::ensure!(
        teams.len() == names.len(),
        ProtectionWriteRefusal::InvalidPushTeams
    );
    Ok(teams.into_iter().map(|team| team.id).collect())
}

#[derive(Debug, sqlx::FromRow)]
pub struct TeamAccess {
    #[sqlx(flatten)]
    pub team: Team,
    pub permission: i16,
    pub org_slug: String,
}
