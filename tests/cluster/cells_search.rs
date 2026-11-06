//! The global search index over the real pinned directory transport: each cell writes only
//! its own repositories, later versions win, and private documents reach only named readers.
use crate::support::native_database::PostgresDatabase;
use codeyard_cells_router::directory::{Client, Listener};
use codeyard_db_core::cells::search::{
    Batch, File, Filter, Index, Kind, Query, Refusal, Results, Version,
};
use codeyard_db_directory::Directory;
use codeyard_model::cells::{Cell, State};
use codeyard_support::{cells::Prefix, node_identity::NodeKey};
use serde_json::json;

fn cell(prefix: Prefix, key: &NodeKey) -> Cell {
    Cell {
        id: prefix,
        name: format!("cell-{prefix}"),
        region: "a".into(),
        state: State::Open,
        web_urls: vec![format!("http://127.0.0.1:{}", 9000 + prefix.get())],
        ssh_addresses: vec![],
        rpc_addresses: vec![],
        node_keys: vec![key.public_hex()],
        database_bytes: 0,
        write_rate: 0,
        maximum_database_bytes: 4_000_000_000_000,
        maximum_write_rate: 10_000,
    }
}
fn repository(id: i64, version: (i64, i64), name: &str, public: bool, readers: &[i64]) -> Batch {
    Batch::Repository {
        id,
        version: Version {
            event: version.0,
            attempt: version.1,
        },
        document: Some(json!({
            "id": id,
            "name": name,
            "full_name": format!("owner{}/{name}", Prefix::for_id(id).unwrap()),
            "owner": {"login": format!("owner{}", Prefix::for_id(id).unwrap())},
            "private": !public,
            "description": format!("about {name}"),
            "stargazers_count": 3,
        })),
        public,
        classic_allowed: true,
        readers: readers.to_vec(),
    }
}
fn query(kind: Kind, viewer: Option<i64>, filter: Filter) -> Query {
    Query {
        kind,
        viewer,
        filter,
        sort: None,
        ascending: false,
        offset: 0,
        limit: 100,
    }
}
fn text(word: &str) -> Filter {
    Filter::Term("text".into(), word.into())
}
async fn names(index: &Client, query: &Query) -> Vec<String> {
    let Results { items, .. } = index.search(query).await.unwrap();
    let mut names: Vec<String> = items
        .iter()
        .map(|hit| hit.document["name"].as_str().unwrap_or("").to_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn cells_write_only_their_own_documents_and_private_ones_reach_only_readers() {
    let registry = PostgresDatabase::unmigrated().await;
    let directory = Directory::connect(&registry.connection_url(), Default::default())
        .await
        .unwrap();
    directory.migrate().await.unwrap();
    let zero_key = NodeKey::generate();
    let one_key = NodeKey::generate();
    let server_key = NodeKey::generate();
    directory
        .register_cell(cell(Prefix::ZERO, &zero_key))
        .await
        .unwrap();
    directory
        .register_cell(cell(Prefix::new(1), &one_key))
        .await
        .unwrap();
    let listener = Listener::bind(
        directory.clone(),
        &server_key,
        "127.0.0.1:0".parse().unwrap(),
        Default::default(),
    )
    .await
    .unwrap();
    let connect = |key: &NodeKey| {
        Client::new(
            key.clone(),
            listener.address,
            server_key.public_key(),
            Default::default(),
        )
        .unwrap()
    };
    let (zero, one) = (connect(&zero_key), connect(&one_key));
    let base = *Prefix::new(1).ids().start();
    let (open, secret, other) = (10, 11, base + 10);
    let reader = 500;

    zero.index(&repository(open, (1, 1), "lantern", true, &[]))
        .await
        .unwrap();
    zero.index(&repository(
        secret,
        (2, 1),
        "lantern-vault",
        false,
        &[reader],
    ))
    .await
    .unwrap();
    one.index(&repository(other, (1, 1), "lantern-far", true, &[]))
        .await
        .unwrap();
    // A cell cannot write another cell's repository, nor a document that claims another id.
    assert!(one
        .index(&repository(open, (9, 9), "hijacked", true, &[]))
        .await
        .is_err());
    let mut lying = repository(open, (9, 9), "lying", true, &[]);
    if let Batch::Repository { document, .. } = &mut lying {
        document.as_mut().unwrap()["private"] = json!(true);
    }
    assert!(zero.index(&lying).await.is_err());

    let all = |viewer| query(Kind::Repositories, viewer, text("lantern"));
    assert_eq!(names(&zero, &all(None)).await, ["lantern", "lantern-far"]);
    assert_eq!(
        names(&one, &all(Some(reader))).await,
        ["lantern", "lantern-far", "lantern-vault"]
    );
    assert_eq!(
        names(&one, &all(Some(reader + 1))).await,
        ["lantern", "lantern-far"]
    );
    let private = query(
        Kind::Repositories,
        Some(reader),
        Filter::All(vec![
            text("lantern"),
            Filter::Term("is".into(), "private".into()),
        ]),
    );
    assert_eq!(names(&zero, &private).await, ["lantern-vault"]);

    // A later version wins; an older one that arrives late changes nothing.
    zero.index(&repository(open, (5, 1), "lantern-new", true, &[]))
        .await
        .unwrap();
    zero.index(&repository(open, (4, 9), "lantern-old", true, &[]))
        .await
        .unwrap();
    assert_eq!(
        names(&zero, &all(None)).await,
        ["lantern-far", "lantern-new"]
    );
    // A removed reader no longer finds the private repository.
    zero.index(&repository(secret, (6, 1), "lantern-vault", false, &[]))
        .await
        .unwrap();
    assert_eq!(
        names(&zero, &all(Some(reader))).await,
        ["lantern-far", "lantern-new"]
    );

    // Issues show only while their repository is visible; a removed repository hides them
    // and a late write cannot bring it back.
    one.index(&Batch::Issue {
        id: base + 20,
        pull: false,
        repository_id: other,
        version: Version {
            event: 2,
            attempt: 1,
        },
        document: Some(json!({"id": base + 20, "name": "issue", "title": "Lantern flickers", "state": "open", "user": {"login": "bob"}})),
    })
    .await
    .unwrap();
    let issues = query(
        Kind::Issues,
        None,
        Filter::All(vec![
            text("flickers"),
            Filter::Term("author".into(), "BOB".into()),
        ]),
    );
    assert_eq!(names(&zero, &issues).await, ["issue"]);

    // Code is stored for the version the repository was written with.
    let file = |content: &str| File {
        path: "src/lamp.rs".into(),
        sha: "a".repeat(40),
        size: content.len() as i64,
        content: content.into(),
    };
    let code = |version: (i64, i64), content: &str| Batch::Code {
        repository_id: other,
        version: Version {
            event: version.0,
            attempt: version.1,
        },
        head: "b".repeat(40),
        files: vec![file(content)],
        first: true,
        last: true,
    };
    assert_eq!(
        one.index(&code((1, 1), "fn wick() {}\nfn needle() {}\n"))
            .await
            .unwrap()
            .code_head,
        Some("b".repeat(40))
    );
    one.index(&code((1, 2), "fn stale() {}\n")).await.unwrap();
    let found = one
        .search(&query(Kind::Code, Some(reader), text("needle")))
        .await
        .unwrap();
    assert_eq!(found.total, 1);
    let hit = found.items[0].code.as_ref().unwrap();
    assert_eq!(
        (hit.path.as_str(), hit.fragments[0].line),
        ("src/lamp.rs", 2)
    );
    assert_eq!(
        one.search(&query(Kind::Code, Some(reader), text("stale")))
            .await
            .unwrap()
            .total,
        0
    );

    one.index(&Batch::Repository {
        id: other,
        version: Version {
            event: 3,
            attempt: 1,
        },
        document: None,
        public: false,
        classic_allowed: true,
        readers: vec![],
    })
    .await
    .unwrap();
    one.index(&repository(other, (2, 7), "lantern-far", true, &[]))
        .await
        .unwrap();
    assert_eq!(names(&zero, &all(None)).await, ["lantern-new"]);
    assert_eq!(names(&zero, &issues).await, Vec::<String>::new());

    // Unsupported qualifiers are refused by the index as well as by the cell.
    let error = zero
        .search(&query(
            Kind::Repositories,
            None,
            Filter::Term("evil".into(), "1".into()),
        ))
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<Refusal>(),
        Some(Refusal::Invalid)
    ));

    directory.reset_search().await.unwrap();
    assert_eq!(names(&zero, &all(None)).await, Vec::<String>::new());
    listener.stop().await;
    directory.close().await;
    drop(registry);
}
