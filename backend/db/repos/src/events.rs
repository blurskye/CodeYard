use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_database::native_operation;
use codeyard_database::DbPool;
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
    pub repository: codeyard_database::models::Repository,
    pub owner: String,
}
/// The originating credential remains part of private activity selection.
#[derive(Clone, Copy)]
pub enum EventViewer<'a> {
    Token {
        user: i64,
        token: i64,
    },
    Feed {
        user: i64,
        token: i64,
        value: &'a str,
    },
}
impl<'a> EventViewer<'a> {
    pub fn parts(self) -> (i64, i64, Option<&'a str>) {
        match self {
            Self::Token { user, token } => (user, token, None),
            Self::Feed { user, token, value } => (user, token, Some(value)),
        }
    }
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
    pub fn parts(self) -> (&'static str, Option<i64>) {
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
native_operation!(crate::postgres::events::record_event,crate::sqlite::events::record_event,
    fn record_event(pool:&DbPool,repo:i64,actor:Option<i64>,kind:&str,payload:&Value,public:bool)->Result<()>);
native_operation!(crate::postgres::events::push_actor,crate::sqlite::events::push_actor,
    fn push_actor(pool:&DbPool,repo:i64,reference:&str,sha:&str)->Result<Option<i64>>);
native_operation!(crate::postgres::events::list_events,crate::sqlite::events::list_events,
    fn list_events(pool:&DbPool,viewer:Option<EventViewer<'_>>,scope:EventScope,public_only:bool,limit:i64,offset:i64)->Result<Vec<EventEntry>>);
native_operation!(crate::postgres::events::activity_feed_token,
    crate::sqlite::events::activity_feed_token,
    fn activity_feed_token(pool:&DbPool, token_id:i64, value:&str)->Result<Option<String>>);
native_operation!(crate::postgres::events::activity_feed_token_id,
    crate::sqlite::events::activity_feed_token_id,
    fn activity_feed_token_id(pool:&DbPool, value:&str)->Result<Option<i64>>);
native_operation!(crate::postgres::events::prune_events,crate::sqlite::events::prune_events,
    fn prune_events(pool:&DbPool)->Result<u64>);
