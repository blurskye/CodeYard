//! A time shown as a date before hydration and a live age after mounting.
use super::{compact_date, relative_age};
#[cfg(target_arch = "wasm32")]
use super::current_time;
use dioxus::prelude::*;

/// The exact time for a hover text: "Oct 2, 2026, 09:30 UTC".
pub fn exact_time(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&chrono::Utc).format("%b %-d, %Y, %H:%M UTC").to_string())
        .unwrap_or_else(|_| value.to_owned())
}

/// A deterministic initial date keeps server and hydration text identical. Changing the
/// timestamp replaces the keyed clock, cancelling its old interval immediately.
#[component]
pub fn LiveTime(value: String, #[props(default)] title: Option<String>) -> Element {
    rsx! { AgeClock { key: "{value}", value, title } }
}

#[component]
fn AgeClock(value: String, title: Option<String>) -> Element {
    let mut now = use_signal(|| None::<chrono::DateTime<chrono::Utc>>);
    let then = chrono::DateTime::parse_from_rfc3339(&value).ok().map(|time| time.with_timezone(&chrono::Utc));
    use_future(move || async move {
        #[cfg(target_arch = "wasm32")]
        if let Some(then) = then {
            loop {
                let clock = current_time();
                now.set(Some(clock));
                let wait = match (clock - then).num_seconds() {
                    ..60 => 1_000,
                    60..3_600 => 30_000,
                    _ => 300_000,
                };
                gloo_timers::future::TimeoutFuture::new(wait).await;
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (then, &mut now);
    });
    let text = match (then, now()) {
        (Some(then), Some(now)) => relative_age(then, now),
        _ => compact_date(&value),
    };
    let title = title.unwrap_or_else(|| exact_time(&value));
    rsx! { time { datetime: value, title, "{text}" } }
}
