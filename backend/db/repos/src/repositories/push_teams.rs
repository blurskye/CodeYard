use crate::repositories::protection::ProtectionWriteRefusal;
use anyhow::Result;
use codeyard_db_core::models::Team;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

pub(crate) fn validate_count(len: usize) -> Result<()> {
    anyhow::ensure!(
        len <= crate::repositories::protection::MAX_ALLOWANCE_ACTORS,
        ProtectionWriteRefusal::InvalidPushTeams
    );
    Ok(())
}

native_operation!(crate::postgres::repositories::push_teams::by_names,
    crate::sqlite::repositories::push_teams::by_names,
    fn by_names(pool: &DbPool, repo: i64, names: &[String]) -> Result<Vec<Team>>);
native_operation!(crate::postgres::repositories::push_teams::by_ids,
    crate::sqlite::repositories::push_teams::by_ids,
    fn by_ids(pool: &DbPool, repo: i64, ids: &[i64]) -> Result<Vec<TeamAccess>>);
native_operation!(crate::postgres::repositories::push_teams::contains_actor,
    crate::sqlite::repositories::push_teams::contains_actor,
    fn contains_actor(pool: &DbPool, repo: i64, actor: i64, ids: &[i64]) -> Result<bool>);

pub async fn resolve_names(pool: &DbPool, repo: i64, names: &[String]) -> Result<Vec<i64>> {
    validate_count(names.len())?;
    let mut names = names.to_vec();
    anyhow::ensure!(
        names
            .iter()
            .all(|name| codeyard_model::account_input::validate_namespace(name, 1).is_ok()),
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
