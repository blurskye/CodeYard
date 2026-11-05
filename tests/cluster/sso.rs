//! Two real web processes complete one organization login against an owned OIDC provider.
use crate::support::database::Database;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Form, Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rsa::{
    signature::{SignatureEncoding, Signer},
    traits::PublicKeyParts,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};

struct Provider {
    issuer: String,
    key: rsa::RsaPrivateKey,
    codes: tokio::sync::Mutex<HashMap<String, HashMap<String, String>>>,
}
async fn authorize(
    State(provider): State<Arc<Provider>>,
    Query(mut query): Query<HashMap<String, String>>,
) -> Response {
    assert_eq!(query["client_id"], "codeyard-organization");
    assert_eq!(query["scope"], "openid profile");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["nonce"], query["code_challenge"]);
    let code = uuid::Uuid::new_v4().to_string();
    let mut redirect = url::Url::parse(&query["redirect_uri"]).unwrap();
    redirect
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &query["state"]);
    query.remove("state");
    provider.codes.lock().await.insert(code, query);
    (StatusCode::FOUND, [("location", redirect.to_string())]).into_response()
}
async fn token(
    State(provider): State<Arc<Provider>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let Some(query) = provider
        .codes
        .lock()
        .await
        .remove(form.get("code").map(String::as_str).unwrap_or(""))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if form.get("client_id").map(String::as_str) != Some("codeyard-organization")
        || form.get("client_secret").map(String::as_str) != Some("owned-client-secret")
        || form.get("redirect_uri") != query.get("redirect_uri")
        || form.get("code_verifier").is_none_or(|v| {
            codeyard_model::oauth_apps::pkce_challenge(v) != query["code_challenge"]
        })
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let now = chrono::Utc::now().timestamp();
    let nonce = if query.get("fixture").is_some_and(|v| v == "wrong-nonce") {
        "other browser"
    } else {
        &query["nonce"]
    };
    let claims = json!({"iss":provider.issuer,"sub":"member-subject","aud":"codeyard-organization",
        "nonce":nonce,"iat":now,"exp":now+300,"name":"OIDC Member","email":"owner@example.test"});
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"org-fixture"}"#);
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let signed = format!("{header}.{body}");
    let signature =
        rsa::pkcs1v15::SigningKey::<Sha256>::new(provider.key.clone()).sign(signed.as_bytes());
    Json(
        json!({"access_token":"unused-provider-access","token_type":"Bearer",
        "id_token":format!("{signed}.{}",URL_SAFE_NO_PAD.encode(signature.to_bytes()))}),
    )
    .into_response()
}
async fn keys(State(provider): State<Arc<Provider>>) -> Json<Value> {
    Json(
        json!({"keys":[{"kty":"RSA","kid":"org-fixture","alg":"RS256","use":"sig",
        "n":URL_SAFE_NO_PAD.encode(provider.key.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(provider.key.e().to_bytes_be())}]}),
    )
}
struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn web(
    db: &Database,
    root: &std::path::Path,
    name: &str,
    public: Option<&str>,
    configuration: &str,
    key: Option<&codeyard_support::node_identity::NodeKey>,
) -> (Server, String) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let url = db.connection_url();
    let (process, addresses) = crate::support::server::spawn_on_free_ports(
        &root.join(format!("{name}.log")),
        2,
        |addresses| {
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_codeyard"));
            command
                .args(["serve"])
                .current_dir(root)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("DATABASE_URL", &url)
                .env("GITSERV_STATE_DIR", root.join(format!("{name}-state")))
                .env("REPOS_PATH", root.join(format!("{name}-repos")))
                .env("HTTP_ADDR", addresses[0].to_string())
                .env("SSH_ADDR", addresses[1].to_string())
                .env("HOST_KEY_PATH", root.join(format!("{name}-host-key")))
                .env(
                    "GITSERV_NODE_ROLES",
                    if key.is_some() { "web" } else { "server" },
                )
                .env(
                    "PUBLIC_URL",
                    public
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("http://{}", addresses[0])),
                )
                .env("COOKIE_SECURE", "false")
                .env("OIDC_PROVIDERS", configuration)
                .env("OIDC_ACME_CLIENT_SECRET", "owned-client-secret")
                .env("SECRETS_MASTER_KEY", "02".repeat(32));
            command.args(["--ssh-mode", "disabled"]);
            if let Some(key) = key {
                command
                    .arg("--cluster")
                    .env("NODE_KEY", key.secret_hex())
                    .env("NODE_NAME", name)
                    .env("CODEYARD_EXPERIMENTAL_CLUSTER", "1")
                    .env("IROH_BIND", "127.0.0.1:0")
                    .env("CLUSTER_TCP_BIND", "127.0.0.1:0");
            }
            command
        },
        |addresses| {
            let client = client.clone();
            async move {
                client
                    .get(format!("http://{}/ready", addresses[0]))
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
            }
        },
    )
    .await;
    (Server(process), format!("http://{}", addresses[0]))
}
async fn flow(client: &reqwest::Client, first: &str, fixture: Option<&str>) -> (String, String) {
    let start = client
        .get(format!("{first}/login/oauth/oidc-acme"))
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), StatusCode::FOUND);
    let cookie = start
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| {
            value
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("; ");
    let mut authorize = url::Url::parse(start.headers()["location"].to_str().unwrap()).unwrap();
    if let Some(fixture) = fixture {
        authorize.query_pairs_mut().append_pair("fixture", fixture);
    }
    let response = client.get(authorize).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    let callback = url::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    (
        format!("{}?{}", callback.path(), callback.query().unwrap()),
        cookie,
    )
}

#[tokio::test]
async fn organization_oidc_crosses_web_nodes_and_rechecks_membership_and_audit_failure() {
    for pg in [false, true] {
        let mut db = Database::new(pg).await;
        db.complete_setup().await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".env"), "").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let key = tokio::task::spawn_blocking(|| {
            rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap()
        })
        .await
        .unwrap();
        let provider = Arc::new(Provider {
            issuer: issuer.clone(),
            key,
            codes: Default::default(),
        });
        let app = Router::new()
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .route("/keys", get(keys))
            .with_state(provider);
        let provider_task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let config = json!([{"organization":"acme","issuer":issuer,"authorization_url":format!("{issuer}/authorize"),
            "token_url":format!("{issuer}/token"),"jwks_url":format!("{issuer}/keys"),"client_id":"codeyard-organization",
            "client_secret_env":"OIDC_ACME_CLIENT_SECRET"}]).to_string();
        let owner = db.actor("sso-owner").await;
        let mut hash = Sha256::new();
        hash.update((issuer.len() as u64).to_be_bytes());
        hash.update(issuer.as_bytes());
        hash.update(b"member-subject");
        let subject = hex::encode(hash.finalize());
        let admitted = codeyard_db_identity::users::transitions::admit_oauth_account(
            db.pool(),
            "oidc-acme",
            &subject,
            "oidc-member",
            None,
        )
        .await
        .unwrap();
        let codeyard_db_identity::users::transitions::AccountAdmission::User(member) = admitted
        else {
            panic!("fixture admission");
        };
        use codeyard_db_orgs::organizations::profiles::{NewOrganization, OrganizationProfile};
        let input =
            NewOrganization::new("acme", OrganizationProfile::new(None, None).unwrap()).unwrap();
        let codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(org) =
            codeyard_db_orgs::organizations::profiles::create_organization(
                db.pool(),
                owner,
                &input,
            )
            .await
            .unwrap()
        else {
            panic!("fixture organization");
        };
        assert!(matches!(
            codeyard_db_orgs::organizations::membership::add_organization_member(
                db.pool(),
                owner,
                org.id,
                member.id,
                codeyard_db_core::models::OrgRole::Member
            )
            .await
            .unwrap(),
            codeyard_db_core::queries::organizations::OrganizationOutcome::Applied(_)
        ));
        let mut node_keys = Vec::new();
        if pg {
            crate::support::native_database::enable_cluster(db.pool())
                .await
                .unwrap();
            for name in ["first", "second"] {
                let key = codeyard_support::node_identity::NodeKey::generate();
                codeyard_db_cluster::cluster::register_own_node(
                    db.pool(),
                    &key.public_hex(),
                    name,
                    &["web".into()],
                    "test",
                )
                .await
                .unwrap();
                node_keys.push(key);
            }
        }
        let (first_server, mut first) =
            web(&db, root.path(), "first", None, &config, node_keys.first()).await;
        let mut first_server = Some(first_server);
        let (second_server, second) = if pg {
            let (server, base) = web(
                &db,
                root.path(),
                "second",
                Some(&first),
                &config,
                node_keys.get(1),
            )
            .await;
            (Some(server), base)
        } else {
            (None, first.clone())
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let (callback, cookie) = flow(&client, &first, None).await;
        let wrong = client
            .get(format!("{second}{callback}"))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
        let response = client
            .get(format!("{second}{callback}"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        let authenticated =
            codeyard_db_identity::users::sessions::list_user_sessions(db.pool(), member.id)
                .await
                .unwrap();
        assert_eq!(authenticated.len(), 1);
        let signed_cookie = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap())
            .find(|value| {
                value
                    .split_once('=')
                    .is_some_and(|(_, value)| value == authenticated[0].id)
            })
            .expect("SSO callback sets its admitted session cookie")
            .to_owned();
        assert_eq!(
            response.status(),
            StatusCode::FOUND,
            "{}",
            response.text().await.unwrap()
        );
        assert_eq!(
            db.scalar(
                "SELECT COUNT(*) FROM sessions WHERE user_id=$1",
                &[member.id]
            )
            .await,
            1
        );
        assert_eq!(
            db.scalar("SELECT COUNT(*) FROM sessions WHERE user_id=$1", &[owner])
                .await,
            0
        );
        assert_eq!(
            db.scalar(
                "SELECT COUNT(*) FROM audit_log WHERE org_id=$1 AND action='org.sso_login'",
                &[org.id]
            )
            .await,
            1
        );
        assert!(!client
            .get(format!("{first}{callback}"))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
            .status()
            .is_redirection());
        if pg {
            // The first web process dies after the acknowledged session transaction.
            drop(first_server.take());
            let page = client
                .get(format!("{second}/settings?tab=profile"))
                .header("cookie", signed_cookie)
                .header("accept", "application/vnd.codeyard.page+json")
                .send()
                .await
                .unwrap();
            assert_eq!(page.status(), StatusCode::OK);
            assert!(page.text().await.unwrap().contains("oidc-member"));
            first = second.clone();
        }
        let (callback, cookie) = flow(&client, &first, Some("wrong-nonce")).await;
        assert_eq!(
            client
                .get(format!("{second}{callback}"))
                .header("cookie", cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_GATEWAY
        );
        let (callback, cookie) = flow(&client, &first, None).await;
        db.execute(
            "DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2",
            &[org.id, member.id],
        )
        .await;
        assert_eq!(
            client
                .get(format!("{second}{callback}"))
                .header("cookie", cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        db.execute(
            "INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'member')",
            &[org.id, member.id],
        )
        .await;
        if pg {
            db.execute("CREATE FUNCTION owned_sso_audit_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='org.sso_login' THEN RAISE EXCEPTION 'owned audit outage'; END IF; RETURN NEW; END $$", &[]).await;
            db.execute("CREATE TRIGGER owned_sso_audit_failure BEFORE INSERT ON audit_log FOR EACH ROW EXECUTE FUNCTION owned_sso_audit_failure()", &[]).await;
        } else {
            db.execute("CREATE TRIGGER owned_sso_audit_failure BEFORE INSERT ON audit_log WHEN NEW.action='org.sso_login' BEGIN SELECT RAISE(ABORT,'owned audit outage'); END", &[]).await;
        }
        let (callback, cookie) = flow(&client, &first, None).await;
        assert_eq!(
            client
                .get(format!("{second}{callback}"))
                .header("cookie", cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            db.scalar(
                "SELECT COUNT(*) FROM sessions WHERE user_id=$1",
                &[member.id]
            )
            .await,
            1
        );
        let (callback, cookie) = flow(&client, &first, None).await;
        db.execute("UPDATE users SET is_banned=true WHERE id=$1", &[member.id])
            .await;
        assert_eq!(
            client
                .get(format!("{second}{callback}"))
                .header("cookie", cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        drop(second_server);
        drop(first_server);
        drop(provider_task);
        db.finish().await;
    }
}
