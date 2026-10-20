//! Site administration: the overview page, and the nodes and storage pages under it.
pub mod nodes;
mod page;
pub mod storage;
#[cfg(test)]
mod tests;
mod users;

pub use page::{admin_page, normalize_days, Account, Activity, Admin, Signup};
pub use users::{admin_users_page, AccountMetadata, AdminUsers};

pub(crate) fn signup_pagination(page: usize, total: usize, base: &str) -> dioxus::prelude::Element {
    use dioxus::prelude::*;
    let pages = total.div_ceil(30).max(1);
    rsx! {
        p { "{total} pending requests" }
        if pages > 1 {
            nav { aria_label: "Signup request pages",
                if page > 1 { a { href: "{base}&request_page={page - 1}", "Previous requests" } }
                span { "Page {page} of {pages}" }
                if page < pages { a { href: "{base}&request_page={page + 1}", "Next requests" } }
            }
        }
    }
}
