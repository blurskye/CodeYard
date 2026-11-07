//! Foreign proofs belong to one operation, never to a shared credential cache.
use super::{freshness::Deadline, Answer, Proof, Query};
use chrono::Utc;
use std::{
    cell::RefCell,
    future::Future,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(super) struct Verified {
    /// The credential as presented, so an expired proof can be asked for again.
    pub proof: Proof,
    pub answer: Answer,
    pub kind: &'static str,
    pub hash: String,
    pub deadline: Deadline,
    /// The home was asked again after expiry and did not confirm; never ask twice.
    pub refused: bool,
}
impl Verified {
    /// A proof the home refused is never current, even before its old deadline.
    pub fn current(&self) -> bool {
        !self.refused && self.deadline.current(Utc::now(), Instant::now())
    }
    fn due(&self, margin: Duration) -> bool {
        !self.refused && self.deadline.remaining(Utc::now(), Instant::now()) <= margin
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

/// Follow-up work that an operation spawns (so a dropped client cannot cancel a write)
/// keeps that operation's proofs with their original deadlines; nothing is extended.
pub fn inherit<F: Future>(work: F) -> impl Future<Output = F::Output> {
    let proofs = PROOFS
        .try_with(|proofs| proofs.borrow().clone())
        .unwrap_or_default();
    PROOFS.scope(RefCell::new(proofs), work)
}

/// Whether a proof is expired or has at most `margin` left.
pub(super) fn any_due(margin: Duration) -> bool {
    PROOFS
        .try_with(|proofs| proofs.borrow().iter().any(|proof| proof.due(margin)))
        .unwrap_or(false)
}

/// Remove and return the due proofs, so asking for them again cannot recurse.
pub(super) fn take_due(margin: Duration) -> Vec<Verified> {
    PROOFS
        .try_with(|proofs| {
            let mut proofs = proofs.borrow_mut();
            let (due, kept) = proofs.drain(..).partition(|proof| proof.due(margin));
            *proofs = kept;
            due
        })
        .unwrap_or_default()
}

/// Put back a proof the home did not confirm. A refused one stays denied and the
/// request's final check answers 503 instead of exposing the response; one the home
/// could not be asked about stays usable until its own deadline.
pub(super) fn restore(mut proof: Verified, refused: bool) {
    proof.refused = refused;
    let _ = PROOFS.try_with(|proofs| {
        let mut proofs = proofs.borrow_mut();
        if !proofs
            .iter()
            .any(|old| old.kind == proof.kind && old.hash == proof.hash)
        {
            proofs.push(proof);
        }
    });
}

pub(super) fn forget(kind: &str, hash: &str) {
    let _ = PROOFS.try_with(|proofs| {
        proofs
            .borrow_mut()
            .retain(|proof| proof.kind != kind || proof.hash != hash)
    });
}

pub fn ensure_current() -> anyhow::Result<()> {
    PROOFS
        .try_with(|proofs| {
            anyhow::ensure!(
                proofs.borrow().iter().all(Verified::current),
                "Identity proof expired"
            );
            Ok(())
        })
        .unwrap_or(Ok(()))
}

pub(super) fn retain(proof: Verified) -> anyhow::Result<()> {
    PROOFS
        .try_with(|proofs| {
            let mut proofs = proofs.borrow_mut();
            // A refused proof stays until the same credential is confirmed again.
            proofs.retain(|old| {
                (old.current() || old.refused)
                    && !(old.kind == proof.kind && old.hash == proof.hash)
            });
            anyhow::ensure!(
                proofs.len() < 4,
                "Too many identity proofs in one operation"
            );
            proofs.push(proof);
            Ok(())
        })
        .map_err(|_| anyhow::anyhow!("Foreign identity requires an operation scope"))?
}

fn sql_text(postgres: bool, value: &str) -> String {
    // Hex literals do not depend on PostgreSQL's standard_conforming_strings setting.
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 15)]));
    }
    if postgres {
        format!("convert_from(decode('{encoded}','hex'),'UTF8')")
    } else {
        format!("CAST(X'{encoded}' AS TEXT)")
    }
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
                    sql_text(postgres, &serde_json::json!({"authority": proof.answer.authority, "write": proof.answer.write}).to_string())
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
