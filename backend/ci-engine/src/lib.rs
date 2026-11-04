//! CI/Actions: workflow triggers queue runs; worker nodes run them with `act`.
pub mod admission;

pub mod events;
mod process;
pub mod push;
pub mod queue;
mod reusable;
pub mod runner_credentials;
pub mod runs;
pub mod schedule;
pub mod workers;

pub mod pull_requests;
mod receive;
pub mod worker;

pub mod code_owners;
pub mod refs;

#[cfg(test)]
mod tests {

    use codeyard_db_core::ci_messages::visible_run_error;

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

pub mod code_owners;
pub mod refs;
