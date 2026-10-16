//! Editor test actors in a disposable database.
use super::{database::Database, db};

impl Database {
    pub async fn actor(&self, name: &str) -> i64 {
        db::users::accounts::create_user(self.pool(), name, None)
            .await
            .unwrap()
            .id
    }
}
