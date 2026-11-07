//! Foreign proofs belong to one operation, never to a shared credential cache.
use super::{freshness::Deadline, Answer, Query};
use chrono::Utc;
use std::{cell::RefCell, future::Future, time::Instant};

#[derive(Clone)]
pub(super) struct Verified {
    pub answer: Answer,
    pub kind: &'static str,
    pub hash: String,
    pub deadline: Deadline,
}
impl Verified {
    pub fn current(&self) -> bool {
        self.deadline.current(Utc::now(), Instant::now())
    }
}

tokio::task_local! {
    static PROOFS: RefCell<Vec<Verified>>;
}

/// An HTTP request, SSH admission or background publication owns its proofs. Nested
/// operations may reuse that scope; spawned tasks must establish their own scope and
/// present the original credential again. Dropping the operation drops all proofs.
pub async fn scope<T>(work: impl Future<Output = T>) -> T {
    PROOFS.scope(RefCell::new(Vec::new()), work).await
}

pub(super) fn retain(proof: Verified) -> anyhow::Result<()> {
    PROOFS
        .try_with(|proofs| {
            let mut proofs = proofs.borrow_mut();
            proofs
                .retain(|old| old.current() && !(old.kind == proof.kind && old.hash == proof.hash));
            anyhow::ensure!(
                proofs.len() < 4,
                "Too many identity proofs in one operation"
            );
            proofs.push(proof);
            Ok(())
        })
        .map_err(|_| anyhow::anyhow!("Foreign identity requires an operation scope"))?
}

/// A bounded, operation-owned SQL relation for existing one-statement permission
/// projections. Only public token bounds are included, never session CSRF values.
pub(super) fn relation(postgres: bool, kind: &str) -> String {
    let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let rows = PROOFS.try_with(|proofs| {
        proofs.borrow().iter().filter(|proof| proof.kind == kind && proof.current())
            .map(|proof| {
                let expiry = quote(&proof.deadline.wall().to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
                let expiry = if postgres { format!("CAST({expiry} AS timestamptz)") } else { expiry };
                let answer = if kind == "token" {
                    serde_json::to_string(&proof.answer).ok().map(|value| quote(&value)).unwrap_or_else(|| "NULL".into())
                } else { "NULL".into() };
                format!("SELECT {} AS kind, CAST({} AS BIGINT) AS credential_id, {} AS credential_hash, CAST({} AS BIGINT) AS user_id, {} AS answer, {} AS expires_at",
                    quote(proof.kind), proof.answer.credential_id.map_or_else(|| "NULL".into(), |id| id.to_string()),
                    quote(&proof.hash), proof.answer.user.id, answer, expiry)
            }).collect::<Vec<_>>()
    }).unwrap_or_default();
    if rows.is_empty() {
        let expiry = if postgres {
            "CAST(NULL AS timestamptz)"
        } else {
            "CAST(NULL AS TEXT)"
        };
        format!("(SELECT CAST(NULL AS TEXT) AS kind, CAST(NULL AS BIGINT) AS credential_id, CAST(NULL AS TEXT) AS credential_hash, CAST(NULL AS BIGINT) AS user_id, CAST(NULL AS TEXT) AS answer, {expiry} AS expires_at WHERE FALSE)")
    } else {
        format!("({})", rows.join(" UNION ALL "))
    }
}

pub(super) fn find(query: &Query<'_>) -> Option<Verified> {
    PROOFS
        .try_with(|proofs| {
            proofs
                .borrow()
                .iter()
                .find(|proof| {
                    proof.current()
                        && proof.kind == query.kind
                        && query
                            .id
                            .is_none_or(|id| proof.answer.credential_id == Some(id))
                        && query.hash.is_none_or(|hash| proof.hash == hash)
                        && (!query.write || proof.answer.write)
                })
                .cloned()
        })
        .ok()
        .flatten()
}
