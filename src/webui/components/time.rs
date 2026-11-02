//! A time shown as its age, such as "20 seconds ago", that keeps counting in the browser.
use super::{compact_date, current_time, relative_age};
use dioxus::prelude::*;

/// The exact time for a hover text: "Oct 2, 2026, 09:30 UTC".
pub fn exact_time(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| {
            time.with_timezone(&chrono::Utc)
                .format("%b %-d, %Y, %H:%M UTC")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_owned())
}

/// An RFC 3339 time as its age, with `title` or the exact time as its hover text. Text that is
/// not a time is shown as a date or as it is. In the browser the age keeps counting: every
/// second in the first minute, then every half minute, then every five minutes.
#[component]
pub fn LiveTime(value: String, #[props(default)] title: Option<String>) -> Element {
    let mut tick = use_signal(|| 0u64);
    let then = chrono::DateTime::parse_from_rfc3339(&value)
        .ok()
        .map(|time| time.with_timezone(&chrono::Utc));
    use_future(move || async move {
        #[cfg(target_arch = "wasm32")]
        if let Some(then) = then {
            loop {
                let age = (current_time() - then).num_seconds();
                let wait = match age {
                    ..60 => 1_000,
                    60..3_600 => 30_000,
                    _ => 300_000,
                };
                gloo_timers::future::TimeoutFuture::new(wait).await;
                tick += 1;
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (then, &mut tick);
    });
    // Reading the tick re-renders the age when it moves on.
    tick();
    let text = then.map_or_else(
        || compact_date(&value),
        |then| relative_age(then, current_time()),
    );
    let title = title.unwrap_or_else(|| exact_time(&value));
    rsx! { time { datetime: value, title, "{text}" } }
}
