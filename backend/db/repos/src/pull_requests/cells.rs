//! The native head owns publication consent; the native base queues refresh work.
use anyhow::Result;
use codeyard_db_core::native_operation;
use codeyard_db_core::{
    cells::pulls::{Admission, Binding, Event, Push},
    models::PullRequest,
    queries::credentials::Actor,
    DbPool,
};

pub use codeyard_db_core::cells::pulls::Refresh;

native_operation!(crate::postgres::pull_requests::cells::subscribe, crate::sqlite::pull_requests::cells::subscribe,
    fn subscribe(pool: &DbPool, binding: &Binding, actor: &Actor, admission: &dyn Admission) -> Result<()>);
native_operation!(crate::postgres::pull_requests::cells::receive_push, crate::sqlite::pull_requests::cells::receive_push,
    fn receive_push(pool: &DbPool, event: &Event, admission: &dyn Admission) -> Result<()>);
native_operation!(crate::postgres::pull_requests::cells::queue_push, crate::sqlite::pull_requests::cells::queue_push,
    /// Queues one push event for a native fork whose foreign base holds a consented pull request.
    fn queue_push(pool: &DbPool, head: i64) -> Result<bool>);
native_operation!(crate::postgres::pull_requests::cells::publisher, crate::sqlite::pull_requests::cells::publisher,
    fn publisher(pool: &DbPool, binding: &Binding) -> Result<Option<i64>>);
native_operation!(crate::postgres::pull_requests::cells::open, crate::sqlite::pull_requests::cells::open,
    fn open(pool: &DbPool, refresh: &Refresh, limit: i64) -> Result<Vec<PullRequest>>);
native_operation!(crate::postgres::pull_requests::cells::bindings, crate::sqlite::pull_requests::cells::bindings,
    fn bindings(pool: &DbPool, head: i64, after: i32, limit: i64) -> Result<Vec<Binding>>);

pub(crate) fn initial(base: i64, head: i64) -> Result<String> {
    let work = Refresh {
        push: Push {
            base_repo_id: base,
            head_repo_id: head,
            version: 0,
        },
        after: 0,
    };
    work.validate()?;
    Ok(serde_json::to_string(&work)?)
}
