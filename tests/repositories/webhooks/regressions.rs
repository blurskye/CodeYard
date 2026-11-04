use super::*;
use codeyard::backend::webhooks::{self, delivery};

async fn insert_hook(database: &Database, url: &str) -> codeyard_db_core::models::Webhook {
    let (owner, id) = try_insert_hook(database, url).await;
    db::webhooks::get_webhook(database.pool(), owner, id.unwrap().unwrap())
        .await
        .unwrap()
        .unwrap()
}

async fn try_insert_hook(
    database: &Database,
    url: &str,
) -> (HookOwner, anyhow::Result<Option<i64>>) {
    let repo = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let owner = HookOwner::Repository(repo.id);
    let id = db::webhooks::insert_webhook(
        database.pool(),
        owner,
        repo.owner_user_id.unwrap(),
        &db::users::accounts::AccountCredential::Session(
            &crate::support::live_session(database.pool(), repo.owner_user_id.unwrap()).await,
        ),
        &WebhookFields {
            username: None,
            password: None,
            payload_url: url.into(),
            content_type: "application/json".into(),
            events: "push".into(),
            active: true,
            ssl_verify: true,
        },
        None,
    )
    .await;
    (owner, id)
}

async fn webhook_settings_batch_statuses_and_paginate(pg: bool) {
    let (mut database, _root, alice, _) = setup(pg).await;
    // Each hook has its own URL, since an equivalent second hook is refused as on GitHub.
    for n in 0..db::webhooks::MAX_HOOKS_PER_OWNER {
        insert_hook(&database, &format!("https://hooks.example/push/{n}")).await;
    }
    let (owner, refused) = try_insert_hook(&database, "https://hooks.example/push/over").await;
    assert!(refused.unwrap_err().is::<db::webhooks::HookLimit>());
    // The settings panel bounds its page even when an owner holds more hooks than the limit,
    // so the rest of the paging fixture goes straight into the table.
    let HookOwner::Repository(repo_id) = owner else {
        panic!("the fixture hooks belong to a repository")
    };
    database
        .execute(
            "INSERT INTO webhooks (repo_id, payload_url, content_type, events, active, ssl_verify, created_by_id) \
             SELECT repo_id, payload_url || '/copy', content_type, events, active, ssl_verify, created_by_id \
             FROM webhooks WHERE repo_id = $1 ORDER BY id",
            &[repo_id],
        )
        .await;
    let hooks = db::webhooks::list_webhooks(database.pool(), owner)
        .await
        .unwrap();
    assert_eq!(hooks.len(), 32);
    let mut record = db::webhooks::NewDelivery {
        redelivery: false,
        guid: uuid::Uuid::new_v4().to_string(),
        event: "push".into(),
        action: None,
        status_code: Some(200),
        duration_ms: 1,
        error: None,
        request_headers: String::new(),
        request_body: String::new(),
        response_headers: None,
        response_body: None,
    };
    db::webhooks::insert_delivery(database.pool(), hooks[1].id, &record)
        .await
        .unwrap();
    record.guid = uuid::Uuid::new_v4().to_string();
    record.status_code = None;
    record.error = Some("failed".into());
    db::webhooks::insert_delivery(database.pool(), hooks[1].id, &record)
        .await
        .unwrap();
    record.guid = uuid::Uuid::new_v4().to_string();
    record.status_code = Some(201);
    db::webhooks::insert_delivery(database.pool(), hooks[2].id, &record)
        .await
        .unwrap();
    let actor =
        db::users::accounts::get_user_by_id(database.pool(), hooks[0].created_by_id.unwrap())
            .await
            .unwrap()
            .unwrap();
    let session = crate::support::live_session(database.pool(), actor.id).await;
    let credential = db::users::accounts::AccountCredential::Session(&session);
    let place = webhooks::panel::Place {
        owner: HookOwner::Repository(hooks[0].repo_id.unwrap()),
        base: "/alice/proj/settings?tab=webhooks",
        action_base: "/alice/proj/_forms",
        csrf_token: "token",
        repository_id: hooks[0].repo_id,
    };
    let first = webhooks::panel::build(
        database.pool(),
        &actor,
        &credential,
        &place,
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(first.hooks.len(), 30);
    assert_eq!(first.hooks[0].last, "No deliveries yet");
    assert_eq!(first.hooks[1].last, "Last delivery failed");
    assert_eq!(first.hooks[2].last, "Last delivery 201");
    assert!(first.next_href.is_some() && first.first_href.is_none());
    let query = [("hook_after".into(), hooks[29].id.to_string())].into();
    let next = webhooks::panel::build(database.pool(), &actor, &credential, &place, &query)
        .await
        .unwrap();
    assert_eq!(next.hooks.len(), 2);
    assert!(next.next_href.is_none() && next.first_href.is_some());
    let query = [
        ("hook".into(), hooks[31].id.to_string()),
        ("edit".into(), "1".into()),
    ]
    .into();
    let editor = webhooks::panel::build(database.pool(), &actor, &credential, &place, &query)
        .await
        .unwrap();
    assert!(editor.hooks.is_empty());
    assert_eq!(editor.editing.unwrap().id, Some(hooks[31].id));
    let (status, html) = alice.page(place.base).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("Next page") && html.contains("hook_after="),
        "{html}"
    );
    database.finish().await;
}

#[tokio::test]
async fn sqlite_webhook_settings_batch_statuses_and_paginate() {
    webhook_settings_batch_statuses_and_paginate(false).await;
}

#[tokio::test]
async fn postgres_webhook_settings_batch_statuses_and_paginate() {
    webhook_settings_batch_statuses_and_paginate(true).await;
}

/// Four sends get a 500 and back off while a fifth, healthy send goes out. Replies after the
/// 500s are held at the receiver until the healthy request has arrived. If a send kept its global
/// slot across its retries and backoffs, the four held sends would keep every slot and the
/// healthy request could not arrive until all their attempts had timed out, more than 30 seconds
/// later. A correct server lets it through at once, or after one held round of retries times out
/// on a very slow runner, so the 30-second wait leaves room.
pub(super) async fn retry_backoff_frees_send_slots(pg: bool) {
    let (mut database, _root, _, _) = setup(pg).await;
    let receiver = Receiver::default();
    receiver.replies.lock().unwrap().extend([500; 4]);
    let held = receiver.hold.clone().write_owned().await;
    let url = receiver.start().await;
    let hook = insert_hook(&database, &url).await;
    let mut sends = Vec::new();
    for _ in 0..4 {
        let pool = database.pool().clone();
        let hook = hook.clone();
        sends.push(tokio::spawn(async move {
            delivery::send(&pool, &hook, &webhooks::outgoing("push", None, &hook, "{}")).await;
        }));
    }
    // Room for a slow CI runner to make the four first attempts.
    tokio::time::timeout(Duration::from_secs(30), async {
        while receiver.received().len() < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(sends.iter().all(|send| !send.is_finished()));
    let healthy = {
        let pool = database.pool().clone();
        let hook = hook.clone();
        tokio::spawn(async move {
            let body = "{\"healthy\":true}";
            delivery::send(&pool, &hook, &webhooks::outgoing("push", None, &hook, body)).await;
        })
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        while !receiver
            .received()
            .iter()
            .any(|request| request.body.contains("healthy"))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("backoff must not retain global send slots");
    drop(held);
    healthy.await.unwrap();
    for send in sends {
        send.await.unwrap();
    }
    assert_eq!(
        db::webhooks::list_deliveries(database.pool(), hook.id)
            .await
            .unwrap()
            .len(),
        5
    );
    database.finish().await;
}

pub(super) async fn queued_delivery_recovers_from_database_failure(pg: bool) {
    let (mut database, _root, _, _) = setup(pg).await;
    let receiver = Receiver::default();
    let url = receiver.start().await;
    let hook = insert_hook(&database, &url).await;
    database
        .execute(
            "ALTER TABLE webhooks RENAME TO temporarily_unavailable_hooks",
            &[],
        )
        .await;
    use tracing::instrument::WithSubscriber;
    let log = crate::support::logs::Log::capture();
    let pool = database.pool().clone();
    let id = hook.id;
    let send = tokio::spawn(
        async move {
            delivery::send(&pool, &hook, &webhooks::outgoing("push", None, &hook, "{}")).await;
        }
        .with_subscriber(log.dispatch.clone()),
    );
    // Restore the table only once the send has met the failure, so the test cannot pass by
    // running the send after the table is back.
    log.wait_for(
        |line| line.contains("could not recheck a queued webhook"),
        "the send never met the missing table",
    )
    .await;
    assert!(
        !send.is_finished(),
        "a database error must not look like a deleted hook"
    );
    database
        .execute(
            "ALTER TABLE temporarily_unavailable_hooks RENAME TO webhooks",
            &[],
        )
        .await;
    send.await.unwrap();
    let recorded = db::webhooks::list_deliveries(database.pool(), id)
        .await
        .unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].status_code, Some(200));
    assert_eq!(receiver.received().len(), 1);
    database.finish().await;
}
