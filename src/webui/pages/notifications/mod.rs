//! The notifications page: `/notifications`.
mod page;

pub(crate) use page::NotificationsView;
pub use page::{
    notifications_href, notifications_page, Notification, NotificationsPage, ReasonOption,
    RepositoryOption, ViewCounts,
};
