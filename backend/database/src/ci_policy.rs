/// Policy refusals from the native CI enqueue and cancellation transactions.
#[derive(Debug, thiserror::Error)]
pub enum CiAdmissionRefusal {
    #[error("Workflow is disabled")]
    WorkflowDisabled,
    #[error("{0}")]
    Missing(&'static str),
    #[error("{0}")]
    Denied(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("{0}")]
    Invalid(&'static str),
}

/// Recorded on queued runs that are cancelled because the person who started them
/// lost access, and shown to the person who looks at the run.
pub const ACCESS_LOST_MESSAGE: &str = "The person who started this run no longer has access";
