// Keep disposable SQL fixture setup outside production backend source. This
// remains a private unit-test module, never an application DB access path.
#[cfg(test)]
#[path = "policy_tests.rs"]
pub mod policy_tests;
#[cfg(test)]
#[path = "prepared_authority.rs"]
mod prepared_authority;
#[cfg(test)]
#[path = "quorum_tests.rs"]
mod quorum_tests;
