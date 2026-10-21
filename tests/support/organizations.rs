//! Helpers for the outcomes of organization changes.
use codeyard::backend::db::organizations::OrganizationOutcome;

/// The value of an organization change that has to apply; any refusal fails the test.
pub fn applied<T: std::fmt::Debug>(outcome: OrganizationOutcome<T>) -> T {
    match outcome {
        OrganizationOutcome::Applied(value) => value,
        other => panic!("expected the organization change to apply: {other:?}"),
    }
}
