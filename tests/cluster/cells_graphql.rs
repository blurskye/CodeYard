//! Router dispatch over real directory TLS and two HTTP cells, without app resolvers in the router.
use crate::support::native_database::PostgresDatabase;
use axum::{
    body::to_bytes,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use codeyard_cells_router::{
    directory,
    graphql::{Plan, Target},
    graphql_dispatch::{Config, Dispatcher},
    http::Transport,
    routes::Routes,
};
use codeyard_db_directory::Directory;
use codeyard_model::{
    cells::{Cell, Key, KeyKind, Object, ObjectKind, State},
    node_id,
};
use codeyard_support::{cells::Prefix, node_identity::NodeKey};
use serde_json::{json, Map, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

struct CellServer {
    stop: CancellationToken,
    task: AbortOnDropHandle<()>,
}
impl Drop for CellServer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
impl CellServer {
    async fn stop(&mut self) {
        self.stop.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

async fn cell(
    prefix: Prefix,
    writes: Arc<Mutex<Vec<u16>>>,
    revoked: Arc<AtomicBool>,
    fail: Arc<AtomicBool>,
) -> (String, CellServer) {
    let app = Router::new().route(
        "/api/graphql",
        post(move |Json(body): Json<Value>| {
            let writes = writes.clone();
            let revoked = revoked.clone();
            let fail = fail.clone();
            async move {
                if revoked.load(Ordering::SeqCst) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                let plan = Plan::parse(body).await.unwrap();
                let mut data = Map::new();
                let mut errors = Vec::new();
                for field in plan.fields {
                    if plan.mutation {
                        if fail.load(Ordering::SeqCst) {
                            return StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        writes.lock().unwrap().push(prefix.get());
                        data.insert(field.key, json!({"clientMutationId":null}));
                        continue;
                    }
                    let value = match field.target {
                        Target::Credential => {
                            json!({"id":node_id::encode("U", *prefix.ids().start())})
                        }
                        Target::Name(owner) => {
                            assert_eq!(
                                owner,
                                if prefix == Prefix::ZERO {
                                    "alice"
                                } else {
                                    "bob"
                                }
                            );
                            json!({"name":format!("repo-{}", prefix.get())})
                        }
                        Target::Node(id) => {
                            assert_eq!(
                                Prefix::for_id(node_id::routed(&id).unwrap().root).unwrap(),
                                prefix
                            );
                            json!({"id":id})
                        }
                        Target::Nodes(ids) => Value::Array(
                            ids.into_iter()
                                .enumerate()
                                .map(|(index, id)| {
                                    if Prefix::for_id(node_id::routed(&id).unwrap().root).unwrap()
                                        == prefix
                                    {
                                        json!({"id":id})
                                    } else {
                                        errors.push(
                                            json!({"message":"Not found","path":[field.key,index]}),
                                        );
                                        Value::Null
                                    }
                                })
                                .collect(),
                        ),
                        target => panic!("Unexpected target: {target:?}"),
                    };
                    data.insert(field.key, value);
                }
                let mut response = json!({"data":data});
                if !errors.is_empty() {
                    response["errors"] = Value::Array(errors);
                }
                Json(response).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stopping.cancelled_owned())
            .await
            .unwrap()
    }));
    (format!("http://{address}"), CellServer { stop, task })
}
async fn response(dispatch: &Dispatcher, headers: &HeaderMap, query: &str) -> Value {
    let response = dispatch
        .execute(headers, json!({"query":query}))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
}
#[tokio::test]
async fn two_cells_split_queries_keep_node_positions_and_validate_before_ordered_mutations() {
    let mut database = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&database.connection_url(), Default::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    let server_key = NodeKey::generate();
    let router_key = NodeKey::generate();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let revoked = Arc::new(AtomicBool::new(false));
    let fail = Arc::new(AtomicBool::new(false));
    let (zero, mut zero_server) = cell(
        Prefix::ZERO,
        writes.clone(),
        revoked.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    let (one, mut one_server) = cell(
        Prefix::new(1),
        writes.clone(),
        Arc::new(AtomicBool::new(false)),
        fail.clone(),
    )
    .await;
    for (prefix, name, url) in [(Prefix::ZERO, "alice", zero), (Prefix::new(1), "bob", one)] {
        let member = NodeKey::generate();
        directory
            .register_cell(Cell {
                id: prefix,
                name: format!("cell-{prefix}"),
                region: "a".into(),
                state: State::Open,
                web_urls: vec![url],
                ssh_addresses: vec![],
                rpc_addresses: vec![],
                node_keys: vec![member.public_hex()],
                database_bytes: 0,
                write_rate: 0,
                maximum_database_bytes: 1000,
                maximum_write_rate: 100,
            })
            .await
            .unwrap();
        let object = Object {
            kind: ObjectKind::User,
            id: *prefix.ids().start(),
        };
        let keys = [Key::new(KeyKind::Name, name).unwrap()];
        let nonce = "a".repeat(64);
        directory
            .reserve(&member.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
        directory
            .confirm(&member.public_hex(), object, &keys, &nonce)
            .await
            .unwrap();
    }
    directory
        .set_router_key(&router_key.public_hex(), true)
        .await
        .unwrap();
    let listener = directory::Listener::bind(
        directory.clone(),
        &server_key,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let routes = Routes::new(
        directory::Client::new(
            router_key,
            listener.address,
            server_key.public_key(),
            Default::default(),
        )
        .unwrap(),
        Default::default(),
    )
    .unwrap();
    // Directory ownership can differ from a row's allocation prefix after a move.
    // No move protocol is exercised here: these are authoritative route fixtures.
    sqlx::query("INSERT INTO directory_objects(kind,id,home_cell,active,nonce_hash) VALUES('repository',99,1,true,$1),('organization',$2,0,true,$1)")
        .bind("a".repeat(64))
        .bind(*Prefix::new(1).ids().start() + 99)
        .execute(&database.raw).await.unwrap();
    for kind in [
        "I", "PR", "IC", "PC", "PRR", "PRRC", "D", "DIC", "L", "M", "RR", "RRT", "EN", "G", "DE",
        "DES", "DPR", "GBP", "RAC", "RI", "IFV", "WF", "WR", "J", "CR", "GIST", "GC",
    ] {
        let id = node_id::scoped(kind, 99, 17).unwrap();
        assert_eq!(
            routes.by_node(&id).await.unwrap().unwrap().id,
            Prefix::new(1)
        );
    }
    let team = node_id::scoped("T", *Prefix::new(1).ids().start() + 99, 17).unwrap();
    assert_eq!(
        routes.by_node(&team).await.unwrap().unwrap().id,
        Prefix::ZERO
    );
    let transport = Transport::new(Default::default()).unwrap();
    let dispatch = Dispatcher::new(routes, transport.clone(), Config::default()).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer cy_pat_0_{}", "a".repeat(64))
            .parse()
            .unwrap(),
    );
    let query = "{a:repository(owner:\"alice\",name:\"repo\"){name} b:repository(owner:\"bob\",name:\"repo\"){name}}";
    let value = response(&dispatch, &headers, query).await;
    assert_eq!(
        value,
        json!({"data":{"a":{"name":"repo-0"},"b":{"name":"repo-1"}}})
    );
    let a = node_id::encode("U", 1);
    let b = node_id::encode("U", *Prefix::new(1).ids().start());
    let value = response(&dispatch, &headers, &format!("{{a:node(id:\"{a}\"){{id}} b:node(id:\"{b}\"){{id}} all:nodes(ids:[\"{b}\",\"{a}\",\"{b}\"]){{id}}}}" )).await;
    assert_eq!(
        value,
        json!({"data":{"a":{"id":a},"b":{"id":b},"all":[{"id":b},{"id":a},{"id":b}]}})
    );
    let reference = node_id::reference(*Prefix::new(1).ids().start(), "refs/heads/main");
    let commit = node_id::git_object("C", *Prefix::new(1).ids().start(), &"a".repeat(40));
    let value = response(
        &dispatch,
        &headers,
        &format!("{{reference:node(id:\"{reference}\"){{id}} commit:node(id:\"{commit}\"){{id}}}}"),
    )
    .await;
    assert_eq!(
        value,
        json!({"data":{"reference":{"id":reference},"commit":{"id":commit}}})
    );
    let a = node_id::encode("I", 1);
    let b = node_id::encode("I", *Prefix::new(1).ids().start());
    let mutation = format!("mutation{{ a:addComment(input:{{subjectId:\"{a}\",body:\"one\"}}){{clientMutationId}} b:addComment(input:{{subjectId:\"{b}\",body:\"two\"}}){{clientMutationId}} c:addComment(input:{{subjectId:\"{a}\",body:\"three\"}}){{clientMutationId}} }}");
    let invalid = mutation.replace("c:addComment", "c:notAMutation");
    let value = response(&dispatch, &headers, &invalid).await;
    assert!(value["errors"]
        .as_array()
        .is_some_and(|errors| !errors.is_empty()));
    assert!(writes.lock().unwrap().is_empty());
    response(&dispatch, &headers, &mutation).await;
    assert_eq!(*writes.lock().unwrap(), vec![0, 1, 0]);
    writes.lock().unwrap().clear();
    fail.store(true, Ordering::SeqCst);
    let reply = dispatch
        .execute(&headers, json!({"query":mutation}))
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        *writes.lock().unwrap(),
        vec![0],
        "a failed second cell does not replay the first mutation or run the third"
    );
    writes.lock().unwrap().clear();
    revoked.store(true, Ordering::SeqCst);
    assert_eq!(
        dispatch
            .execute(&headers, json!({"query":mutation}))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(writes.lock().unwrap().is_empty());
    revoked.store(false, Ordering::SeqCst);
    one_server.stop().await;
    assert!(dispatch
        .execute(&headers, json!({"query":query}))
        .await
        .is_err());
    let value = response(
        &dispatch,
        &headers,
        "{a:repository(owner:\"alice\",name:\"repo\"){name}}",
    )
    .await;
    assert_eq!(value["data"]["a"]["name"], "repo-0");
    transport.shutdown().await;
    zero_server.stop().await;
    listener.stop().await;
    directory.close().await;
    database.finish().await;
}
