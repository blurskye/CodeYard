//! The global search index over the real pinned directory transport: each cell writes only
//! its own repositories, later versions win, and private documents reach only named readers.
use crate::support::native_database::PostgresDatabase;
use codeyard_database::cells::search::{
    Batch, File, Filter, Index, Kind, Package, Query, Refusal, Results, Version,
};
use codeyard_db_directory::Directory;
use codeyard_router::directory::{Client, Listener};
use codeyard_support::node_identity::NodeKey;
use codeyard_types::cell_ids::Prefix;
use codeyard_types::cells::{Cell, State};
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
        read_regions: Vec::new(),
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
        packages: Vec::new(),
    }
}
fn with_packages(mut batch: Batch, list: &[(&str, bool)]) -> Batch {
    if let Batch::Repository { packages, .. } = &mut batch {
        *packages = list
            .iter()
            .map(|(name, provided)| Package {
                ecosystem: "npm".into(),
                name: (*name).into(),
                provided: *provided,
            })
            .collect();
    }
    batch
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
async fn blobs(pool: &codeyard_database::pool::PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT content FROM search_blobs ORDER BY content")
        .fetch_all(pool)
        .await
        .unwrap()
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
        comments: String::new(),
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

    // File text is stored once per content, and text no file uses any more is removed.
    let twin = base + 11;
    one.index(&repository(twin, (1, 1), "twin", true, &[]))
        .await
        .unwrap();
    let twin_code = |content: &str| Batch::Code {
        repository_id: twin,
        version: Version {
            event: 1,
            attempt: 1,
        },
        head: "c".repeat(40),
        files: vec![file(content)],
        first: true,
        last: true,
    };
    one.index(&twin_code("fn wick() {}\nfn needle() {}\n"))
        .await
        .unwrap();
    assert_eq!(
        blobs(&registry.raw).await,
        ["fn wick() {}\nfn needle() {}\n"]
    );
    assert_eq!(
        one.search(&query(Kind::Code, None, text("needle")))
            .await
            .unwrap()
            .total,
        2
    );
    one.index(&twin_code("fn twin() {}\n")).await.unwrap();
    assert_eq!(
        blobs(&registry.raw).await,
        ["fn twin() {}\n", "fn wick() {}\nfn needle() {}\n"]
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
        packages: vec![],
    })
    .await
    .unwrap();
    assert_eq!(blobs(&registry.raw).await, ["fn twin() {}\n"]);
    one.index(&repository(other, (2, 7), "lantern-far", true, &[]))
        .await
        .unwrap();
    assert_eq!(names(&zero, &all(None)).await, ["lantern-new"]);
    assert_eq!(names(&zero, &issues).await, Vec::<String>::new());

    // "Used by" lists public dependents in any cell of the package a repository provides.
    let dependent = base + 30;
    zero.index(&with_packages(
        repository(open, (7, 1), "lantern-new", true, &[]),
        &[("lantern-lib", true)],
    ))
    .await
    .unwrap();
    one.index(&with_packages(
        repository(dependent, (1, 1), "wick", true, &[]),
        &[("lantern-lib", false), ("left-pad", false)],
    ))
    .await
    .unwrap();
    // A private dependent is never listed, not even to its reader, and keeps no packages.
    zero.index(&with_packages(
        repository(secret, (8, 1), "lantern-vault", false, &[reader]),
        &[("lantern-lib", false)],
    ))
    .await
    .unwrap();
    let uses = |viewer| {
        query(
            Kind::Repositories,
            viewer,
            Filter::Term("uses".into(), open.to_string()),
        )
    };
    assert_eq!(names(&zero, &uses(None)).await, ["wick"]);
    assert_eq!(names(&one, &uses(Some(reader))).await, ["wick"]);
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM search_packages WHERE repository_id=$1")
            .bind(secret)
            .fetch_one(&registry.raw)
            .await
            .unwrap();
    assert_eq!(rows, 0);
    // A dependent that turns private, or a provider that stops providing, drops out.
    one.index(&with_packages(
        repository(dependent, (2, 1), "wick", false, &[]),
        &[("lantern-lib", false)],
    ))
    .await
    .unwrap();
    assert_eq!(names(&zero, &uses(None)).await, Vec::<String>::new());
    one.index(&with_packages(
        repository(dependent, (3, 1), "wick", true, &[]),
        &[("lantern-lib", false)],
    ))
    .await
    .unwrap();
    assert_eq!(names(&zero, &uses(None)).await, ["wick"]);
    zero.index(&repository(open, (8, 1), "lantern-new", true, &[]))
        .await
        .unwrap();
    assert_eq!(names(&zero, &uses(None)).await, Vec::<String>::new());
    // A hostile package name is refused before it reaches the index.
    assert!(zero
        .index(&with_packages(
            repository(open, (9, 1), "lantern-new", true, &[]),
            &[("Upper\0", true)],
        ))
        .await
        .is_err());
    assert!(zero
        .search(&query(
            Kind::Repositories,
            None,
            Filter::Term("uses".into(), "-1".into()),
        ))
        .await
        .is_err());

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
    ranked_words(&zero, &one).await;
    listener.stop().await;
    directory.close().await;
    drop(registry);
}

/// Runs through pinned TLS, including query validation and private counts, not just SQL.
async fn ranked_words(zero: &Client, one: &Client) {
    let foreign = *Prefix::new(1).ids().start() + 70;
    zero.index(&repository(70, (1, 1), "word-library", true, &[]))
        .await
        .unwrap();
    one.index(&repository(foreign, (1, 1), "word-secret", false, &[500]))
        .await
        .unwrap();
    // Deliberately give the higher rank a later ID: rank must beat the stable ID order.
    for (id, root, pull, title, body, comments) in [
        (71, 70, false, "Other", "quartz", ""),
        (72, 70, true, "Quartz quartz quartz", "", ""),
        (73, 70, false, "Quartz", "", ""),
        (74, 70, false, "Other", "", "quartz"),
        (
            75,
            foreign,
            true,
            "Quartz quartz quartz quartz",
            "privateword",
            "secretword",
        ),
        (76, 70, false, "Quartz", "", ""),
    ] {
        let client = if root == foreign { one } else { zero };
        client
            .index(&Batch::Issue {
                id,
                repository_id: root,
                pull,
                version: Version {
                    event: 1,
                    attempt: 1,
                },
                document: Some(json!({"id":id,"title":title,"body":body})),
                comments: comments.into(),
            })
            .await
            .unwrap();
    }
    let mut words = query(Kind::Issues, None, text("QUARTZ"));
    let found = zero.search(&words).await.unwrap();
    assert_eq!(found.total, 5);
    assert_eq!(
        found.items.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        [72, 73, 76, 71, 74]
    );
    assert!(found.items[0].score > found.items[1].score);
    assert!(found.items[1].score > found.items[3].score);
    assert!(found.items[3].score > found.items[4].score);
    assert!(found
        .items
        .iter()
        .all(|hit| hit.document.get("comments").is_none()));
    // Paging is deterministic, including ties, and explicit sort is the secondary order.
    words.limit = 1;
    words.sort = Some("created".into());
    words.ascending = true;
    for (offset, id) in [72, 73, 76, 71, 74].into_iter().enumerate() {
        words.offset = offset as u32;
        assert_eq!(zero.search(&words).await.unwrap().items[0].id, id);
    }
    for viewer in [None, Some(501)] {
        for term in ["privateword", "secretword"] {
            let found = zero
                .search(&query(Kind::Issues, viewer, text(term)))
                .await
                .unwrap();
            assert_eq!(found.total, 0);
            assert!(found.items.is_empty());
        }
    }
    assert_eq!(
        zero.search(&query(Kind::Issues, Some(500), text("secretword")))
            .await
            .unwrap()
            .total,
        1
    );
    // Prose is words, not substrings; repository names still use substring matching.
    assert_eq!(
        zero.search(&query(Kind::Issues, None, text("quar")))
            .await
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        zero.search(&query(Kind::Repositories, None, text("libr")))
            .await
            .unwrap()
            .total,
        1
    );
    for (field, total) in [("title", 3), ("body", 1), ("comments", 1)] {
        let filter = Filter::All(vec![
            text("quartz"),
            Filter::Term("in".into(), field.into()),
        ]);
        assert_eq!(
            zero.search(&query(Kind::Issues, None, filter))
                .await
                .unwrap()
                .total,
            total
        );
    }
    // Raw punctuation/operators are data, never to_tsquery or SQL syntax. Empty lexemes
    // match nothing; a very long value is refused without exposing private text.
    for hostile in ["'\" & | ! :* <-> ( )", "'); DROP TABLE search_issues; --"] {
        assert_eq!(
            zero.search(&query(Kind::Issues, None, text(hostile)))
                .await
                .unwrap()
                .total,
            0
        );
    }
    let error = zero
        .search(&query(Kind::Issues, None, text(&"q".repeat(100_000))))
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<Refusal>(),
        Some(Refusal::Invalid)
    ));
    // Editing/deleting comments replaces their indexed words rather than accumulating them.
    zero.index(&Batch::Issue {
        id: 74,
        repository_id: 70,
        pull: false,
        version: Version {
            event: 2,
            attempt: 1,
        },
        document: Some(json!({"id":74,"title":"Other"})),
        comments: String::new(),
    })
    .await
    .unwrap();
    assert_eq!(
        zero.search(&query(Kind::Issues, None, text("quartz")))
            .await
            .unwrap()
            .total,
        4
    );
}
