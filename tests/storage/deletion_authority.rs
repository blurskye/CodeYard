use crate::support::database::Database;

use codeyard_database::models::FileReason;
use codeyard_database::models::FileVisibility;

async fn restricted_account_cannot_delete(pg: bool) {
    let mut database = Database::new(pg).await;
    let pool = database.pool();
    let user = codeyard_db_identity::users::accounts::create_user(pool, "owner", None)
        .await
        .unwrap();
    let file = codeyard_db_repos::files::reserve_upload(
        pool,
        codeyard_db_repos::files::ReserveUpload {
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
        database
            .execute(
                "UPDATE users SET is_banned=false,must_change_password=false WHERE id=$1",
                &[user.id],
            )
            .await;
        database
            .execute(
                &format!("UPDATE users SET {restriction}=true WHERE id=$1"),
                &[user.id],
            )
            .await;
        assert!(matches!(
            codeyard_db_repos::files::delete_file(pool, file.id, user.id)
                .await
                .unwrap(),
            codeyard_db_repos::files::DeleteFileOutcome::Denied
        ));
        assert!(codeyard_db_repos::files::get_file_by_id(pool, file.id)
            .await
            .unwrap()
            .is_some());
    }
    database.finish().await;
}

#[tokio::test]
async fn deletion_rechecks_account_restrictions_on_sqlite() {
    restricted_account_cannot_delete(false).await;
}

#[tokio::test]
async fn deletion_rechecks_account_restrictions_on_postgres() {
    restricted_account_cannot_delete(true).await;
}
