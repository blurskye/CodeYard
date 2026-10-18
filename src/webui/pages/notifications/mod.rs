//! The notifications page: `/notifications`.
mod page;

pub(crate) use page::NotificationsView;
pub use page::{
    body_preview, notifications_href, notifications_page, safe_notification_link, Notification,
    NotificationsPage, ReasonOption, RepositoryOption, ViewCounts,
};
