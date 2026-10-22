//! The administration overview: sign-up requests that wait for a decision, and daily activity
//! charts. Counts with a page of their own (users, storage) are not repeated here.
use super::nav::{admin_layout, Section};
use crate::webui::components::charts::{
    activity_series, chart_grid, timeframe_links, DailyActivity, Series,
};
use crate::webui::components::format_bytes;
use crate::webui::navigation::NavLink;
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Admin {
    /// Sign-up requests waiting for a decision. `None` on a public node, which has none.
    pub pending_requests: Option<usize>,
    /// One point per day, oldest first.
    pub activity: Vec<DailyActivity>,
    /// The bytes stored each day, one for each point of `activity`.
    pub storage_bytes: Vec<i64>,
    pub days: i32,
    pub version: String,
}

pub fn admin_page(model: Admin) -> Element {
    let dates: Vec<String> = model
        .activity
        .iter()
        .map(|point| point.date.clone())
        .collect();
    let mut series = activity_series(&model.activity);
    series.push(Series {
        bytes: true,
        ..Series::new(
            "storage",
            "Storage added",
            format_bytes,
            model.storage_bytes,
        )
    });
    let pending = model.pending_requests.filter(|pending| *pending > 0);
    admin_layout(
        Section::Overview,
        "Overview",
        &format!(
            "Activity on this installation, which runs Codeyard {}.",
            model.version
        ),
        rsx! {
            if let Some(pending) = pending {
                div { class: "identity-admin-attention", role: "status",
                    p {
                        if pending == 1 { "1 sign-up request is" } else { "{pending} sign-up requests are" }
                        " waiting for a decision."
                    }
                    NavLink { href: "/admin/users", class: "btn btn-sm", "Review" }
                }
            }
            section { class: "identity-admin-activity", aria_labelledby: "admin-activity-title",
                header { class: "identity-admin-panel-header",
                    h2 { id: "admin-activity-title", "Activity" }
                    {timeframe_links("/admin", model.days)}
                }
                if model.activity.is_empty() {
                    div { class: "identity-empty", h3 { "No activity yet" } p { "Activity for this timeframe will appear here." } }
                } else {
                    {chart_grid(series, &dates)}
                }
            }
        },
    )
}
