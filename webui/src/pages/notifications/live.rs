use dioxus::prelude::*;

#[component]
pub(crate) fn NotificationUpdates() -> Element {
    use_future(move || async move {
        #[cfg(target_arch = "wasm32")]
        receive().await;
    });
    rsx! {}
}

#[cfg(target_arch = "wasm32")]
async fn receive() {
    use futures_util::{
        future::{poll_fn, select, Either},
        Sink, StreamExt,
    };
    use gloo_net::websocket::{futures::WebSocket, Message, State};
    use gloo_timers::future::TimeoutFuture;
    use std::pin::Pin;

    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(location) = window.location().href() else {
        return;
    };
    let Ok(mut url) = url::Url::parse(&location) else {
        return;
    };
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        _ => return,
    };
    if url.set_scheme(scheme).is_err() {
        return;
    }
    url.set_path("/api/v3/notifications/ws");
    url.set_query(None);
    url.set_fragment(None);
    let mut delay = 500;
    loop {
        if let Ok(mut socket) = WebSocket::open(url.as_str()) {
            let connected = {
                let ready = poll_fn(|context| Pin::new(&mut socket).poll_ready(context));
                let timeout = TimeoutFuture::new(5000);
                futures_util::pin_mut!(ready, timeout);
                matches!(select(ready, timeout).await, Either::Left((Ok(()), _)))
            };
            if connected && matches!(socket.state(), State::Open) {
                delay = 500;
                // Reload after connecting as well: hints sent during a gap are not history.
                crate::navigation::refresh_page();
                while let Some(Ok(Message::Text(_))) = socket.next().await {
                    crate::navigation::refresh_page();
                }
            }
        }
        TimeoutFuture::new(delay).await;
        delay = (delay * 2).min(30000);
    }
}
