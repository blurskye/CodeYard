//! Durable deployment jobs commit their inventory and terminal result in one transaction.
//! A process that stops mid-job rolls back to pending; another web node can safely resume it.
use crate::backend::db;
use codeyard_db_core::DbPool;
use std::time::Duration;
use tokio::sync::watch;

pub async fn run(pool: DbPool, mut stop: watch::Receiver<bool>) {
    loop {
        if *stop.borrow() {
            return;
        }
        let result = tokio::select! {
            biased;
            changed = stop.changed() => {if changed.is_err() || *stop.borrow() {return;} else {continue;}},
            result = db::artifact_metadata::process_job(&pool,None) => result,
        };
        match result {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => tracing::warn!(error=%error,"Artifact metadata job queue unavailable"),
        }
        tokio::select! {
            changed = stop.changed() => {if changed.is_err() || *stop.borrow() {return;}},
            () = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}
