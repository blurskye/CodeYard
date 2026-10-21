use super::*;
use codeyard::backend::webhooks::{self, delivery};

async fn insert_hook(database: &Database, url: &str) -> db::models::Webhook {
    let repo = db::repositories::records::get_repository(database.pool(), "alice", "proj")
        .await
        .unwrap()
        .unwrap();
    let owner = HookOwner::Repository(repo.id);
    let id = db::webhooks::insert_webhook(
        database.pool(),
        owner,
        repo.owner_user_id.unwrap(),
        &WebhookFields {
            payload_url: url.into(),
            content_type: "application/json".into(),
            events: "push".into(),
            active: true,
            ssl_verify: true,
        },
        None,
    )
    .await
    .unwrap()
    .unwrap();
    db::webhooks::get_webhook(database.pool(), owner, id)
        .await
        .unwrap()
        .unwrap()
}

async fn webhook_settings_batch_statuses_and_paginate(pg: bool) {
    let (mut database, _root, alice, _) = setup(pg).await;
    let mut hooks = Vec::new();
    for _ in 0..32 {
        hooks.push(insert_hook(&database, "https://hooks.example/push").await);
    }
    let mut record = db::webhooks::NewDelivery {
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
    let place = webhooks::panel::Place {
        owner: HookOwner::Repository(hooks[0].repo_id.unwrap()),
        base: "/alice/proj/settings?tab=webhooks",
        action_base: "/alice/proj/_forms",
        csrf_token: "token",
        repository_id: hooks[0].repo_id,
    };
    let first = webhooks::panel::build(database.pool(), &actor, &place, &Default::default())
        .await
        .unwrap();
    assert_eq!(first.hooks.len(), 30);
    assert_eq!(first.hooks[0].last, "No deliveries yet");
    assert_eq!(first.hooks[1].last, "Last delivery failed");
    assert_eq!(first.hooks[2].last, "Last delivery 201");
    assert!(first.next_href.is_some() && first.first_href.is_none());
    let query = [("hook_after".into(), hooks[29].id.to_string())].into();
    let next = webhooks::panel::build(database.pool(), &actor, &place, &query)
        .await
        .unwrap();
    assert_eq!(next.hooks.len(), 2);
    assert!(next.next_href.is_none() && next.first_href.is_some());
    let query = [
        ("hook".into(), hooks[31].id.to_string()),
        ("edit".into(), "1".into()),
    ]
    .into();
    let editor = webhooks::panel::build(database.pool(), &actor, &place, &query)
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

pub(super) async fn retry_backoff_frees_send_slots(pg: bool) {
    let (mut database, _root, _, _) = setup(pg).await;
    let receiver = Receiver::default();
    receiver.replies.lock().unwrap().extend([500; 4]);
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
    tokio::time::timeout(Duration::from_secs(5), async {
        while receiver.received().len() < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(sends.iter().all(|send| !send.is_finished()));
    tokio::time::timeout(
        Duration::from_millis(500),
        delivery::send(
            database.pool(),
            &hook,
            &webhooks::outgoing("push", None, &hook, "{\"healthy\":true}"),
        ),
    )
    .await
    .expect("backoff must not retain global send slots");
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
    let pool = database.pool().clone();
    let id = hook.id;
    let send = tokio::spawn(async move {
        delivery::send(&pool, &hook, &webhooks::outgoing("push", None, &hook, "{}")).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
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
