//! Site administration: the overview page, and the nodes and storage pages under it.
pub mod nodes;
mod page;
pub mod storage;
#[cfg(test)]
mod tests;
mod users;

pub use page::{admin_page, normalize_days, Account, Activity, Admin, Signup};
pub use users::{admin_users_page, AdminUsers};
