//! Database-clock lease and policy evaluation; external effects use add and drain.
use anyhow::Result;
use codeyard_db_core::{native_operation, queries::credentials::Actor, DbPool};
use codeyard_support::cluster::ScalingPolicy;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub role: String,
    pub count: i32,
    pub drain_node: Option<String>,
    pub dry_run: bool,
}

native_operation!(crate::postgres::scaling::policies, crate::sqlite::scaling::policies,
    fn policies(pool: &DbPool) -> Result<Vec<ScalingPolicy>>);
native_operation!(crate::postgres::scaling::save, crate::sqlite::scaling::save,
    fn save(pool: &DbPool, actor: &Actor, policy: &ScalingPolicy) -> Result<()>);
native_operation!(crate::postgres::scaling::evaluate, crate::sqlite::scaling::evaluate,
    /// At most one action per minute for the installation, while the lease owner is live.
    fn evaluate(pool: &DbPool, node: i64) -> Result<Option<Action>>);

pub(crate) fn decode(rows: Vec<String>) -> Result<Vec<ScalingPolicy>> {
    let mut policies = ["web", "ssh", "storage", "runner"]
        .map(ScalingPolicy::defaults)
        .to_vec();
    for raw in rows {
        let policy: ScalingPolicy = serde_json::from_str(&raw)?;
        policy.validate().map_err(anyhow::Error::msg)?;
        let index = policies
            .iter()
            .position(|item| item.role == policy.role)
            .ok_or_else(|| anyhow::anyhow!("invalid scaling role"))?;
        policies[index] = policy;
    }
    Ok(policies)
}

pub(crate) fn add_count(policy: &ScalingPolicy, active: i32, reserved: i32, trigger: bool) -> i32 {
    let capacity = policy
        .maximum
        .saturating_sub(active)
        .saturating_sub(reserved)
        .max(0);
    let needed = policy
        .minimum
        .saturating_sub(active)
        .saturating_sub(reserved)
        .max(0);
    if needed > 0 {
        needed.min(policy.step).min(capacity)
    } else if trigger {
        policy.step.min(capacity)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservations_and_maximum_bound_every_add() {
        let mut policy = ScalingPolicy::defaults("runner");
        policy.minimum = 2;
        policy.maximum = 3;
        policy.step = 10;
        assert_eq!(add_count(&policy, 0, 1, false), 1);
        assert_eq!(add_count(&policy, 2, 1, true), 0);
        assert_eq!(add_count(&policy, 2, 0, true), 1);
        assert_eq!(add_count(&policy, 4, 0, true), 0);
    }
}
