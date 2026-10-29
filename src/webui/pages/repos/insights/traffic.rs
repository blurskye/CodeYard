use crate::webui::{navigation::NavLink, pages::repos::chrome::RepoChrome};
use dioxus::prelude::*;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub weekly: bool,
    pub views: Series,
    pub clones: Series,
    pub paths: Vec<Path>,
    pub referrers: Vec<Referrer>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Series {
    pub count: i64,
    pub uniques: i64,
    pub buckets: Vec<Bucket>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Bucket {
    pub timestamp: String,
    pub count: i64,
    pub uniques: i64,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Path {
    pub path: String,
    pub title: String,
    pub count: i64,
    pub uniques: i64,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Referrer {
    pub referrer: String,
    pub count: i64,
    pub uniques: i64,
}
fn series(title: &str, data: &Series, weekly: bool) -> Element {
    let bucket = if weekly {
        "Week starting Monday (UTC)"
    } else {
        "Day (UTC)"
    };
    rsx! {
        section { class: "panel p-4",
            h3 { "{title}" }
            p { strong { "{data.count} total" } " · {data.uniques} unique visitors across the period" }
            if data.buckets.is_empty() {
                p { class: "text-muted", "No observations in this period." }
            } else {
                table { class: "repo-traffic-table",
                    thead { tr { th { "{bucket}" } th { "Count" } th { "Unique visitors" } } }
                    tbody {
                        for row in &data.buckets {
                            tr {
                                td { time { datetime: row.timestamp.clone(),{crate::webui::components::compact_date(&row.timestamp)} } }
                                td { "{row.count}" }
                                td { "{row.uniques}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
pub fn render(repo: &RepoChrome, path: &str, page: &Page) -> Element {
    let base = repo.base();
    let prefix = format!("{base}/");
    rsx! {
        div { class: "repo-insights-content repo-traffic",
            section { class: "panel p-4",
                h2 { "Repository traffic" }
                p { "The last 14 UTC dates, including today. Unique visitors are distinct across the period, not the sum of daily counts." }
                form { method: "get", action: path, class: "repo-traffic-filter",
                    label { "Group observations by "
                        select { name: "per", class:"form-select",
                            option { value: "day", selected: !page.weekly, "Day" }
                            option { value: "week", selected: page.weekly, "Week" }
                        }
                    }
                    button { class: "btn btn-sm", r#type: "submit", "Apply" }
                }
                p { class: "text-muted", "Observed content-page navigations and initial Git object transfers, not negotiated fetches, API reads or background polls. Settings and confidential security reports are excluded. Empty clones without an object transfer are not counted." }
                p { class: "text-muted", "Client addresses are not stored. Repository-scoped, daily keyed fingerprints deduplicate visitors. Only people with push access can view traffic." }
            }
            {series("Views", &page.views, page.weekly)}
            {series("Initial Git object transfers", &page.clones, page.weekly)}
            section { class: "panel p-4",
                h3 { "Popular content" }
                if page.paths.is_empty() {
                    p { class: "text-muted", "No content observations in this period." }
                } else {
                    table { class: "repo-traffic-table",
                        thead { tr { th { "Content" } th { "Views" } th { "Unique visitors" } } }
                        tbody {
                            for row in &page.paths {
                                tr {
                                    td {
                                        if row.path.starts_with(&prefix) || row.path == base {
                                            NavLink { href: row.path.clone(), "{row.title}" }
                                        } else {
                                            span { "{row.title}" }
                                        }
                                        code { "{row.path}" }
                                    }
                                    td { "{row.count}" }
                                    td { "{row.uniques}" }
                                }
                            }
                        }
                    }
                }
            }
            section { class: "panel p-4",
                h3 { "Referring sites" }
                p { class: "text-muted", "External hostnames only; referral paths, query strings and credentials are not retained." }
                if page.referrers.is_empty() {
                    p { class: "text-muted", "No external referrers in this period." }
                } else {
                    table { class: "repo-traffic-table",
                        thead { tr { th { "Site" } th { "Views" } th { "Unique visitors" } } }
                        tbody {
                            for row in &page.referrers {
                                tr { td { "{row.referrer}" } td { "{row.count}" } td { "{row.uniques}" } }
                            }
                        }
                    }
                }
            }
        }
    }
}
