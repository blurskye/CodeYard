//! Live run metadata and cursor-based log updates; normal navigation still loads the full page.
#[cfg(any(target_arch = "wasm32", test))]
use super::ActionsPage;
use super::{logs::LogLine, run::RunProgress, Deployment};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunLogUpdate {
    pub job_id: String,
    pub logs: Vec<LogLine>,
    pub next_cursor: i64,
    pub has_more: bool,
    pub workflow_sha: Option<String>,
    pub progress: RunProgress,
    pub deployment: Option<Deployment>,
}

#[cfg(any(target_arch = "wasm32", test))]
fn apply_update(model: &mut ActionsPage, update: RunLogUpdate) {
    let Some(selected) = model
        .selected
        .as_mut()
        .filter(|selected| selected.run.id == update.job_id)
    else {
        return;
    };
    let cursor = selected.logs.last().map_or(0, |line| line.id);
    selected
        .logs
        .extend(update.logs.into_iter().filter(|line| line.id > cursor));
    selected.fetched_lines = selected.logs.len();
    let mut bytes: usize = selected
        .logs
        .iter()
        .map(|line| serde_json::to_vec(line).map_or(0, |json| json.len() + 1))
        .sum();
    let mut remove = 0;
    while selected.logs.len() - remove > model.log_limit || bytes > 8 * 1024 * 1024 {
        bytes = bytes.saturating_sub(
            serde_json::to_vec(&selected.logs[remove]).map_or(0, |json| json.len() + 1),
        );
        remove += 1;
    }
    if remove > 0 {
        selected.logs.drain(..remove);
        selected.logs_truncated = true;
    }
    selected.deployment = update.deployment;
    // Drain outstanding history before terminal status unmounts the poller.
    if !update.has_more {
        selected.run.status = update.progress.status;
        selected.run.started_at = update.progress.started_at;
        selected.run.completed_at = update.progress.completed_at;
        selected.run.error_message = update.progress.error_message;
        for run in &mut model.runs {
            if run.id == selected.run.id {
                *run = selected.run.clone();
            }
        }
    }
}

#[component]
pub(super) fn ActiveRunPoll(run_id: String) -> Element {
    let failure = use_signal(|| false);
    #[cfg(target_arch = "wasm32")]
    {
        let mut failure = failure;
        let crate::webui::app::CurrentPage(mut current) = use_context();
        let controller =
            use_hook(|| web_sys::AbortController::new().expect("browser abort controller"));
        let on_drop = controller.clone();
        use_drop(move || on_drop.abort());
        use_future(move || {
            let run_id = run_id.clone();
            let controller = controller.clone();
            async move {
                loop {
                    gloo_timers::future::TimeoutFuture::new(2000).await;
                    let (location, cursor) = {
                        let page = current.peek();
                        let crate::webui::app::Page::Actions(
                            crate::webui::pages::ci::Page::Actions(model),
                        ) = &page.page
                        else {
                            break;
                        };
                        let Some(selected) = model
                            .selected
                            .as_ref()
                            .filter(|selected| selected.run.id == run_id)
                        else {
                            break;
                        };
                        (
                            page.location.clone(),
                            selected.logs.last().map_or(0, |line| line.id),
                        )
                    };
                    let url = format!("/api/ci/jobs/{run_id}/history?after={cursor}&limit=500");
                    let request = async {
                        let response = gloo_net::http::Request::get(&url)
                            .abort_signal(Some(&controller.signal()))
                            .send()
                            .await
                            .map_err(|_| ())?;
                        if !response.ok() {
                            return Err(());
                        }
                        response.json::<RunLogUpdate>().await.map_err(|_| ())
                    };
                    let timeout = gloo_timers::future::TimeoutFuture::new(10_000);
                    futures_util::pin_mut!(request, timeout);
                    let result = match futures_util::future::select(request, timeout).await {
                        futures_util::future::Either::Left((result, _)) => result,
                        futures_util::future::Either::Right(_) => {
                            controller.abort();
                            failure.set(true);
                            break;
                        }
                    };
                    if current.peek().location != location {
                        continue;
                    }
                    match result {
                        Ok(update) => {
                            let mut page = current.write();
                            if let crate::webui::app::Page::Actions(
                                crate::webui::pages::ci::Page::Actions(model),
                            ) = &mut page.page
                            {
                                apply_update(model, update);
                            }
                            failure.set(false);
                        }
                        Err(()) => failure.set(true),
                    }
                }
            }
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = run_id;
    rsx! {
        if failure() { p { role: "status", "Live updates are unavailable. Refresh to try again." } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_updates_deduplicate_bound_logs_and_drain_before_terminal_status() {
        let mut model = super::super::tests::model();
        let run = model.runs[0].clone();
        model.selected = Some(super::super::SelectedRun {
            run,
            logs: vec![],
            deployment: None,
            fetched_lines: 0,
            logs_truncated: false,
            planned: Vec::new(),
        });
        model.log_limit = 2;
        let update = |ids: &[i64], more| RunLogUpdate {
            job_id: "real-run".into(),
            logs: ids
                .iter()
                .map(|id| LogLine {
                    id: *id,
                    stage: "test".into(),
                    line: "output".into(),
                    level: "info".into(),
                    timestamp: 0,
                })
                .collect(),
            next_cursor: *ids.last().unwrap(),
            has_more: more,
            workflow_sha: None,
            progress: RunProgress {
                status: "success".into(),
                started_at: Some(1),
                completed_at: Some(2),
                error_message: None,
            },
            deployment: None,
        };
        apply_update(&mut model, update(&[1, 2], true));
        assert_eq!(model.selected.as_ref().unwrap().run.status, "running");
        apply_update(&mut model, update(&[2, 3], false));
        let selected = model.selected.as_ref().unwrap();
        assert_eq!(
            selected.logs.iter().map(|line| line.id).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(selected.logs_truncated);
        assert_eq!(selected.run.status, "success");
        assert_eq!(model.runs[0].status, "success");
    }
}
