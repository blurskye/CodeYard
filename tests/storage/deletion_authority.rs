use crate::support::native_database::{NativeDatabase, PostgresDatabase};
use codeyard::backend::db::{
    self, files,
    models::{FileReason, FileVisibility},
    DbPool,
};

async fn restricted_account_cannot_delete(pool: &DbPool) {
    let user = db::users::accounts::create_user(pool, "owner", None)
        .await
        .unwrap();
    let file = files::reserve_upload(
        pool,
        files::ReserveUpload {
            user_id: user.id,
            filename: "avatar.png",
            content_type: "image/png",
            file_size: 8,
            reason: FileReason::Avatar,
            visibility: FileVisibility::Public,
            sha256_hex: &"a".repeat(64),
        },
    )
    .await
    .unwrap();
    for restriction in ["is_banned", "must_change_password"] {
        match pool {
            DbPool::Postgres(raw) => {
                sqlx::query(
                    "UPDATE users SET is_banned=false,must_change_password=false WHERE id=$1",
                )
                .bind(user.id)
                .execute(raw)
                .await
                .unwrap();
                sqlx::query(&format!("UPDATE users SET {restriction}=true WHERE id=$1"))
                    .bind(user.id)
                    .execute(raw)
                    .await
                    .unwrap();
            }
            DbPool::Sqlite(raw) => {
                sqlx::query(
                    "UPDATE users SET is_banned=false,must_change_password=false WHERE id=?",
                )
                .bind(user.id)
                .execute(raw)
                .await
                .unwrap();
                sqlx::query(&format!("UPDATE users SET {restriction}=true WHERE id=?"))
                    .bind(user.id)
                    .execute(raw)
                    .await
                    .unwrap();
            }
        }
        assert!(matches!(
            files::delete_file(pool, file.id, user.id).await.unwrap(),
            files::DeleteFileOutcome::Denied
        ));
        assert!(files::get_file_by_id(pool, file.id)
            .await
            .unwrap()
            .is_some());
    }
}

#[tokio::test]
async fn deletion_rechecks_account_restrictions_on_sqlite() {
    let db = NativeDatabase::new().await;
    restricted_account_cannot_delete(&db.pool).await;
}

#[tokio::test]
async fn deletion_rechecks_account_restrictions_on_postgres() {
    let db = PostgresDatabase::new().await;
    restricted_account_cannot_delete(&db.pool).await;
}
