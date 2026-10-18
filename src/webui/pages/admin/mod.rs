//! Site administration: the overview page, and the nodes and storage pages under it.
pub mod nodes;
mod page;
pub mod storage;
#[cfg(test)]
mod tests;

pub use page::{
    admin_page, ban_confirmation_page, normalize_days, Account, Activity, Admin, Signup,
};
