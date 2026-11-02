use crate::backend::db::users::{
    accounts::AccountCredential,
    social_accounts::{SocialAccount, TooManyAccounts},
};
use anyhow::Result;
use sqlx::PgPool;

pub async fn list_social_accounts(pool: &PgPool, user: i64) -> Result<Vec<SocialAccount>> {
    Ok(sqlx::query_as(
        "SELECT provider,url FROM user_social_accounts WHERE user_id=$1 ORDER BY url",
    )
    .bind(user)
    .fetch_all(pool)
    .await?)
}

pub async fn change_social_accounts(
    pool: &PgPool,
    user: i64,
    credential: &AccountCredential<'_>,
    accounts: &[SocialAccount],
    add: bool,
) -> Result<Vec<SocialAccount>> {
    let mut tx = pool.begin().await?;
    super::transitions::require_account_exclusive(&mut tx, user, credential).await?;
    for account in accounts {
        if add {
            sqlx::query("INSERT INTO user_social_accounts(user_id,provider,url) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                .bind(user).bind(&account.provider).bind(&account.url).execute(&mut *tx).await?;
        } else {
            sqlx::query("DELETE FROM user_social_accounts WHERE user_id=$1 AND url=$2")
                .bind(user)
                .bind(&account.url)
                .execute(&mut *tx)
                .await?;
        }
    }
    let accounts: Vec<SocialAccount> = sqlx::query_as(
        "SELECT provider,url FROM user_social_accounts WHERE user_id=$1 ORDER BY url",
    )
    .bind(user)
    .fetch_all(&mut *tx)
    .await?;
    anyhow::ensure!(accounts.len() <= 4, TooManyAccounts);
    tx.commit().await?;
    Ok(accounts)
}
