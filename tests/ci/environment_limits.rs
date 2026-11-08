use crate::support::database::Database;

use codeyard_db_actions::environment_values as values;
use codeyard_db_actions::environment_values::Save;
use codeyard_db_actions::environment_values::ValueError;
use codeyard_db_orgs::organization_values::Kind;
use codeyard_git_storage::repositories::lifecycle;
use codeyard_security::secrets::SecretEncryptor;

async fn limits(pg: bool) {
    let mut f = Database::new(pg).await;
    f.complete_setup().await;
    let owner = f.actor("limit_owner").await;
    let outsider = f.actor("limit_outsider").await;
    let root = tempfile::tempdir().unwrap();
    let repo = lifecycle::provision(
        (f.pool(), None),
        root.path(),
        codeyard_db_repos::repositories::creation::RepositoryCreationOwner::personal(owner),
        "limits",
        None,
        true,
        None,
    )
    .await
    .unwrap();
    let encryptor = SecretEncryptor::new(&[0x63; 32]);
    for environment in ["count", "bytes", "race"] {
        values::create(f.pool(), owner, repo.id, environment)
            .await
            .unwrap();
    }
    for name in [
        "".to_owned(),
        " ".to_owned(),
        "control\nname".to_owned(),
        "x".repeat(256),
    ] {
        assert!(matches!(
            values::create(f.pool(), owner, repo.id, &name).await,
            Err(ValueError::InvalidEnvironment)
        ));
    }
    values::create(f.pool(), owner, repo.id, &"🦀".repeat(255))
        .await
        .unwrap();
    for kind in [Kind::Secret, Kind::Variable] {
        for index in 0..100 {
            values::save(
                f.pool(),
                Some(&encryptor),
                owner,
                repo.id,
                "count",
                &Save {
                    kind,
                    previous: None,
                    name: &format!("VALUE_{index}"),
                    value: Some(""),
                },
            )
            .await
            .unwrap();
        }
        assert!(matches!(
            values::save(
                f.pool(),
                Some(&encryptor),
                owner,
                repo.id,
                "count",
                &Save {
                    kind,
                    previous: None,
                    name: "OVER_LIMIT",
                    value: Some("value")
                }
            )
            .await,
            Err(ValueError::Limit)
        ));
        assert!(matches!(
            values::save(
                f.pool(),
                Some(&encryptor),
                outsider,
                repo.id,
                "count",
                &Save {
                    kind,
                    previous: None,
                    name: "DENIED",
                    value: Some("value")
                }
            )
            .await,
            Err(ValueError::Denied)
        ));
    }
    values::save(
        f.pool(),
        Some(&encryptor),
        owner,
        repo.id,
        "count",
        &Save {
            kind: Kind::Secret,
            previous: None,
            name: "VALUE_0",
            value: Some("replacement"),
        },
    )
    .await
    .unwrap();
    values::save(
        f.pool(),
        None,
        owner,
        repo.id,
        "count",
        &Save {
            kind: Kind::Variable,
            previous: Some("VALUE_0"),
            name: "RENAMED",
            value: None,
        },
    )
    .await
    .unwrap();
    let value = "x".repeat(48 * 1024);
    for index in 0..21 {
        values::save(
            f.pool(),
            Some(&encryptor),
            owner,
            repo.id,
            "bytes",
            &Save {
                kind: Kind::Secret,
                previous: None,
                name: &format!("SECRET_{index}"),
                value: Some(&value),
            },
        )
        .await
        .unwrap();
    }
    assert!(matches!(
        values::save(
            f.pool(),
            Some(&encryptor),
            owner,
            repo.id,
            "bytes",
            &Save {
                kind: Kind::Secret,
                previous: None,
                name: "TOO_LARGE",
                value: Some(&value)
            }
        )
        .await,
        Err(ValueError::SecretsSize)
    ));
    assert_eq!(
        values::list(f.pool(), owner, repo.id, "bytes", Kind::Secret)
            .await
            .unwrap()
            .len(),
        21
    );
    for index in 0..5 {
        values::save(
            f.pool(),
            None,
            owner,
            repo.id,
            "bytes",
            &Save {
                kind: Kind::Variable,
                previous: None,
                name: &format!("VARIABLE_{index}"),
                value: Some(&value),
            },
        )
        .await
        .unwrap();
    }
    assert!(matches!(
        values::save(
            f.pool(),
            None,
            owner,
            repo.id,
            "bytes",
            &Save {
                kind: Kind::Variable,
                previous: None,
                name: "TOO_LARGE",
                value: Some(&value)
            }
        )
        .await,
        Err(ValueError::Size)
    ));
    assert_eq!(
        values::list(f.pool(), owner, repo.id, "bytes", Kind::Variable)
            .await
            .unwrap()
            .len(),
        5
    );
    for kind in [Kind::Secret, Kind::Variable] {
        for index in 0..99 {
            values::save(
                f.pool(),
                Some(&encryptor),
                owner,
                repo.id,
                "race",
                &Save {
                    kind,
                    previous: None,
                    name: &format!("VALUE_{index}"),
                    value: Some(""),
                },
            )
            .await
            .unwrap();
        }
        let a = Save {
            kind,
            previous: None,
            name: "LAST_A",
            value: Some(""),
        };
        let b = Save {
            kind,
            previous: None,
            name: "LAST_B",
            value: Some(""),
        };
        let (a, b) = tokio::join!(
            values::save(f.pool(), Some(&encryptor), owner, repo.id, "race", &a),
            values::save(f.pool(), Some(&encryptor), owner, repo.id, "race", &b)
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert!(matches!(a, Err(ValueError::Limit)) || matches!(b, Err(ValueError::Limit)));
        assert_eq!(
            values::list(f.pool(), owner, repo.id, "race", kind)
                .await
                .unwrap()
                .len(),
            100
        );
    }
    // Each aggregate budget has room for exactly one more 12 KiB value.
    let value = "x".repeat(12 * 1024);
    for (kind, previous_count) in [(Kind::Secret, 21), (Kind::Variable, 5)] {
        let a = Save {
            kind,
            previous: None,
            name: "BYTES_A",
            value: Some(&value),
        };
        let b = Save {
            kind,
            previous: None,
            name: "BYTES_B",
            value: Some(&value),
        };
        let (a, b) = tokio::join!(
            values::save(f.pool(), Some(&encryptor), owner, repo.id, "bytes", &a),
            values::save(f.pool(), Some(&encryptor), owner, repo.id, "bytes", &b)
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert!(match kind {
            Kind::Secret =>
                matches!(a, Err(ValueError::SecretsSize))
                    || matches!(b, Err(ValueError::SecretsSize)),
            Kind::Variable =>
                matches!(a, Err(ValueError::Size)) || matches!(b, Err(ValueError::Size)),
        });
        assert_eq!(
            values::list(f.pool(), owner, repo.id, "bytes", kind)
                .await
                .unwrap()
                .len(),
            previous_count + 1
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn sqlite_environment_values_limits() {
    limits(false).await;
}
#[tokio::test]
async fn postgres_environment_values_limits() {
    limits(true).await;
}
