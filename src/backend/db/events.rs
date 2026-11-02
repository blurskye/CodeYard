//! Persisted repository activity, independent of webhook subscriptions.
use super::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct Event {
    #[sqlx(rename = "event_id")]
    pub id: i64,
    pub repo_id: i64,
    pub actor_id: Option<i64>,
    pub kind: String,
    pub payload: Value,
    pub is_public: bool,
    #[sqlx(rename = "event_created_at")]
    pub created_at: DateTime<Utc>,
}
#[derive(Debug, Clone, FromRow)]
pub struct EventEntry {
    #[sqlx(flatten)]
    pub event: Event,
    #[sqlx(flatten)]
    pub repository: super::models::Repository,
    pub owner: String,
}
#[derive(Clone, Copy)]
pub enum EventScope {
    All,
    Repository(i64),
    Network(i64),
    Organization(i64),
    Actor(i64),
    Received(i64),
}
impl EventScope {
    pub(crate) fn parts(self) -> (&'static str, Option<i64>) {
        match self {
            Self::All => ("all", None),
            Self::Repository(id) => ("repo", Some(id)),
            Self::Network(id) => ("network", Some(id)),
            Self::Organization(id) => ("org", Some(id)),
            Self::Actor(id) => ("actor", Some(id)),
            Self::Received(id) => ("received", Some(id)),
        }
    }
}
native_operation!(crate::backend::db::postgres::events::record_event,crate::backend::db::sqlite::events::record_event,
    fn record_event(pool:&DbPool,repo:i64,actor:Option<i64>,kind:&str,payload:&Value,public:bool)->Result<()>);
native_operation!(crate::backend::db::postgres::events::push_actor,crate::backend::db::sqlite::events::push_actor,
    fn push_actor(pool:&DbPool,repo:i64,reference:&str,sha:&str)->Result<Option<i64>>);
native_operation!(crate::backend::db::postgres::events::list_events,crate::backend::db::sqlite::events::list_events,
    fn list_events(pool:&DbPool,viewer:Option<i64>,scope:EventScope,public_only:bool,limit:i64,offset:i64)->Result<Vec<EventEntry>>);
native_operation!(crate::backend::db::postgres::events::activity_feed_token,
    crate::backend::db::sqlite::events::activity_feed_token,
    fn activity_feed_token(pool:&DbPool, token_id:i64, value:&str)->Result<Option<String>>);
native_operation!(crate::backend::db::postgres::events::activity_feed_token_id,
    crate::backend::db::sqlite::events::activity_feed_token_id,
    fn activity_feed_token_id(pool:&DbPool, value:&str)->Result<Option<i64>>);
native_operation!(crate::backend::db::postgres::events::prune_events,crate::backend::db::sqlite::events::prune_events,
    fn prune_events(pool:&DbPool)->Result<u64>);
