//! The profile page: `/:username`, with the repository list, the stars tab and the activity feed.
mod page;
#[cfg(test)]
mod tests;

pub(crate) use page::{profile_icon, ProfileIcon};
pub use page::{
    profile_page, Profile, ProfileActivity, ProfileActivityItem, ProfileRepositories,
    ProfileStarList, ProfileStars, Repository,
};
