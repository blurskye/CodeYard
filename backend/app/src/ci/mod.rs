//! CI/Actions: workflow triggers queue runs; worker nodes run them with `act`.
use codeyard_db_core::ci_messages::visible_run_error;
pub mod admission;

pub mod config;

pub mod environment_values;
pub mod events;
pub mod logs;
pub mod oidc;
mod process;
pub mod push;
pub mod queue;
mod reusable;
pub mod runner_credentials;
mod runner_labels;
pub mod runs;
pub mod runtime;
pub mod schedule;
pub mod tokens;
pub mod workers;

pub mod worker;
pub mod workflows;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runners_error_text_is_replaced_and_the_servers_own_messages_pass() {
        assert_eq!(visible_run_error(None), None);
        assert_eq!(
            visible_run_error(Some("Cancelled")).as_deref(),
            Some("Cancelled")
        );
        assert_eq!(
            visible_run_error(Some("CI could not run: clone failed at /srv/git/x.git")).as_deref(),
            Some("The runner recorded an error")
        );
    }
}
