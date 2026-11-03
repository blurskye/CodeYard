//! The notifications page: `/notifications`.
mod live;
mod page;

pub(crate) use live::NotificationUpdates;
pub(crate) use page::NotificationsView;
pub use page::{
    notifications_href, notifications_page, Notification, NotificationsPage, ReasonOption,
    RepositoryOption, ViewCounts,
};
