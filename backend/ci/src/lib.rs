//! CI/Actions: workflow triggers queue runs; worker nodes run them with `act`.
pub mod admission;
pub mod code_owners;
pub mod config;
pub mod environment_values;
pub mod events;
pub mod logs;
pub mod oidc;
mod process;
pub mod pull_requests;
pub mod push;
pub mod queue;
mod receive;
pub mod refs;
mod reusable;
pub mod runner_credentials;
pub mod runner_labels;
pub mod runs;
pub mod runtime;
pub mod schedule;
pub mod tokens;
pub mod worker;
pub mod workers;

#[cfg(test)]
mod tests {

    use codeyard_database::ci_messages::visible_run_error;

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
