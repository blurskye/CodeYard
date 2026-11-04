use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// The workflow, or Actions for the repository, is turned off: nothing runs, and event
    /// paths skip it quietly.
    #[error("{0}")]
    WorkflowDisabled(&'static str),
    #[error("{0}")]
    Missing(&'static str),
    #[error("{0}")]
    Denied(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("CI is unavailable right now; check run history before retrying")]
    Unavailable,
}

impl From<anyhow::Error> for AdmissionError {
    fn from(error: anyhow::Error) -> Self {
        let error = match error.downcast::<Self>() {
            Ok(error) => return error,
            Err(error) => error,
        };
        match error.downcast::<crate::ci_policy::CiAdmissionRefusal>() {
            Ok(crate::ci_policy::CiAdmissionRefusal::WorkflowDisabled) => {
                Self::WorkflowDisabled("Workflow is disabled")
            }
            Ok(crate::ci_policy::CiAdmissionRefusal::Missing(message)) => Self::Missing(message),
            Ok(crate::ci_policy::CiAdmissionRefusal::Denied(message)) => Self::Denied(message),
            Ok(crate::ci_policy::CiAdmissionRefusal::Conflict(message)) => Self::Conflict(message),
            Ok(crate::ci_policy::CiAdmissionRefusal::Invalid(message)) => Self::Invalid(message),
            Err(_) => Self::Unavailable,
        }
    }
}

impl From<tokio::time::error::Elapsed> for AdmissionError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::Unavailable
    }
}

impl From<RepoError> for AdmissionError {
    fn from(error: RepoError) -> Self {
        match error.kind {
            ErrorKind::Invalid | ErrorKind::MissingPath => {
                Self::Invalid("CI revision or workflow is invalid")
            }
            ErrorKind::NotFound | ErrorKind::Conflict => Self::Conflict("CI ref unavailable"),
            ErrorKind::Unavailable => Self::Unavailable,
        }
    }
}
