use super::shell::{shell_icon, ShellIcon};
use crate::webui::document::Theme;
use dioxus::prelude::*;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ThemeSaved {
    pub theme: Theme,
}

fn describe(theme: Theme) -> (&'static str, ShellIcon) {
    match theme {
        Theme::System => ("System", ShellIcon::Monitor),
        Theme::Light => ("Light", ShellIcon::Sun),
        Theme::Dark => ("Dark", ShellIcon::Moon),
    }
}

/// The theme button. The icon shows the current theme and one press saves the next one: system,
/// dark, light, system. Without scripts it is a plain form post. The header draws it as an icon;
/// `menu` draws the phone copy in the navigation menu, as a row with its label.
#[component]
pub fn ThemeForm(
    csrf_token: String,
    return_to: String,
    theme: Theme,
    #[props(default)] menu: bool,
) -> Element {
    let notice = use_signal(String::new);
    let next = theme.next();
    #[cfg(target_arch = "wasm32")]
    let (form_token, form_target) = (csrf_token.clone(), return_to.clone());
    let (current_label, icon) = describe(theme);
    let (next_label, _) = describe(next);
    let hint = format!("Theme: {current_label}. Switch to {next_label}.");
    let (form_class, button_class) = if menu {
        ("site-theme-toggle site-theme-menu", "site-menu-link")
    } else {
        (
            "site-theme-toggle",
            "site-icon-button icon-tooltip icon-tooltip-end",
        )
    };
    rsx! {
        form { action: "/_forms/theme", method: "post", class: form_class,
            onsubmit: move |event| {
                event.prevent_default();
                #[cfg(target_arch = "wasm32")]
                spawn(save_theme(form_token.clone(), form_target.clone(), next, notice));
            },
            input { r#type: "hidden", name: "csrf_token", value: csrf_token }
            input { r#type: "hidden", name: "return_to", value: return_to }
            input { r#type: "hidden", name: "theme", value: next.as_str() }
            button { r#type: "submit", class: button_class, "aria-label": hint,
                {shell_icon(icon)}
                if menu { span { "Theme: {current_label}" } }
            }
            span { class: "site-sr-only", role: "status", "Theme: {current_label}" }
            span { class: "site-theme-error", role: "alert", "{notice}" }
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn save_theme(
    csrf_token: String,
    return_to: String,
    choice: Theme,
    mut notice: Signal<String>,
) {
    notice.set(String::new());
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer
        .append_pair("csrf_token", &csrf_token)
        .append_pair("return_to", &return_to)
        .append_pair("theme", choice.as_str());
    let request = gloo_net::http::Request::post("/_forms/theme")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", crate::webui::bootstrap::MODEL_CONTENT_TYPE)
        .credentials(web_sys::RequestCredentials::SameOrigin)
        .body(serializer.finish());
    let response = match request {
        Ok(request) => request.send().await,
        Err(_) => {
            notice.set("Appearance could not be saved. Please try again.".into());
            return;
        }
    };
    match response {
        Ok(response)
            if response.ok()
                && response.headers().get("Content-Type").as_deref()
                    == Some(crate::webui::bootstrap::THEME_CONTENT_TYPE) =>
        {
            let saved = response
                .text()
                .await
                .ok()
                .and_then(|json| serde_json::from_str::<ThemeSaved>(&json).ok());
            let Some(saved) = saved else {
                notice
                    .set("The appearance update could not be confirmed. Reload the page.".into());
                return;
            };
            if let Some(crate::webui::app::CurrentPage(mut current)) =
                try_consume_context::<crate::webui::app::CurrentPage>()
            {
                if let Some(crate::webui::app::AppearanceEpoch(mut epoch)) =
                    try_consume_context::<crate::webui::app::AppearanceEpoch>()
                {
                    let next = *epoch.peek() + 1;
                    epoch.set(next);
                }
                // Appearance is committed separately from the page's editor/revision
                // snapshot; never re-read or reset drafts.
                current.write().chrome.theme = saved.theme;
            }
            notice.set(String::new());
        }
        Ok(response) => notice.set(format!(
            "The appearance update could not be confirmed (HTTP {}). Reload the page and try again.",
            response.status()
        )),
        Err(_) => notice.set("Appearance could not be saved. Please try again.".into()),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn render(theme: Theme) -> String {
        dioxus_ssr::render_element(rsx! {
            ThemeForm { csrf_token: "csrf-1".to_string(), return_to: "/alice".to_string(), theme }
        })
    }

    #[test]
    fn the_button_posts_the_next_theme_in_order() {
        for (current, next) in [
            (Theme::System, "dark"),
            (Theme::Dark, "light"),
            (Theme::Light, "system"),
        ] {
            let html = render(current);
            assert!(html.contains("action=\"/_forms/theme\""));
            assert!(html.contains("value=\"csrf-1\""));
            assert!(html.contains("value=\"/alice\""));
            assert!(
                html.contains(&format!("name=\"theme\" value=\"{next}\"")),
                "{current:?} should post {next}: {html}"
            );
            assert_eq!(html.matches("<button").count(), 1);
            assert!(!html.contains("<select"));
        }
    }

    #[test]
    fn the_button_names_the_current_and_next_theme() {
        let html = render(Theme::Dark);
        assert!(html.contains("aria-label=\"Theme: Dark. Switch to Light.\""));
        assert!(html.contains("Theme: Dark"));
    }

    #[test]
    fn the_menu_row_is_labelled_and_posts_the_next_theme() {
        let html = dioxus_ssr::render_element(rsx! {
            ThemeForm { csrf_token: "csrf-1".to_string(), return_to: "/alice".to_string(), theme: Theme::Dark, menu: true }
        });
        assert!(html.contains("class=\"site-theme-toggle site-theme-menu\""));
        assert!(html.contains("class=\"site-menu-link\""));
        assert!(html.contains("Theme: Dark"));
        assert!(html.contains("name=\"theme\" value=\"light\""));
        assert_eq!(html.matches("<button").count(), 1);
    }
}
