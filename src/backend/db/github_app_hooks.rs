//! App-owned webhook settings and delivery history; native repo/org hooks remain unchanged.
use crate::backend::db::{
    models::{Webhook, WebhookDelivery},
    webhooks::{NewDelivery, SealedSecret, WebhookPatch},
    DbPool,
};
use anyhow::Result;
#[derive(Debug, sqlx::FromRow)]
pub struct Delivery {
    #[sqlx(flatten)]
    pub recorded: WebhookDelivery,
    pub redelivery: bool,
    pub installation_id: Option<i64>,
    pub repository_id: Option<i64>,
    pub url: String,
}
#[derive(sqlx::FromRow)]
pub struct Subscription {
    #[sqlx(flatten)]
    pub hook: Webhook,
    pub installation_id: i64,
    pub permissions: String,
}
native_operation!(crate::backend::db::postgres::github_app_hooks::hook,crate::backend::db::sqlite::github_app_hooks::hook,
    fn hook(pool:&DbPool,app:i64)->Result<Option<Webhook>>);
native_operation!(crate::backend::db::postgres::github_app_hooks::save,crate::backend::db::sqlite::github_app_hooks::save,
    /// Saves an app's webhook. Fields the patch leaves `None` keep their stored values, so edits
    /// of different fields never undo each other. Answers false when the app has no webhook yet
    /// and the patch does not name every field a new one needs.
    fn save(pool:&DbPool,app:i64,patch:&WebhookPatch,secret:Option<&SealedSecret>,clear:bool)->Result<bool>);
native_operation!(crate::backend::db::postgres::github_app_hooks::for_repository,crate::backend::db::sqlite::github_app_hooks::for_repository,
    fn for_repository(pool:&DbPool,repo:i64)->Result<Vec<Subscription>>);
native_operation!(crate::backend::db::postgres::github_app_hooks::subscription,crate::backend::db::sqlite::github_app_hooks::subscription,
    fn subscription(pool:&DbPool,repo:i64,app:i64,installation:i64,automatic:bool)->Result<Option<Subscription>>);
native_operation!(crate::backend::db::postgres::github_app_hooks::insert_delivery,crate::backend::db::sqlite::github_app_hooks::insert_delivery,
    fn insert_delivery(pool:&DbPool,app:i64,record:&NewDelivery,redelivery:bool,installation:Option<i64>,repo:Option<i64>,url:&str)->Result<()>);
native_operation!(crate::backend::db::postgres::github_app_hooks::deliveries,crate::backend::db::sqlite::github_app_hooks::deliveries,
    fn deliveries(pool:&DbPool,app:i64)->Result<Vec<Delivery>>);
native_operation!(crate::backend::db::postgres::github_app_hooks::delivery,crate::backend::db::sqlite::github_app_hooks::delivery,
    fn delivery(pool:&DbPool,app:i64,id:i64)->Result<Option<Delivery>>);
