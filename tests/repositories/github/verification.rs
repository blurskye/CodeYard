use super::{json, protection::call, Router, Value};

pub(super) async fn run(router: &Router, base: &str, writer: &str, sha: &str) {
    let unsigned = json!({"verified":false,"reason":"unsigned","signature":null,"payload":null,"verified_at":null});
    let commit = call(
        router,
        "GET",
        &format!("{base}/git/commits/{sha}"),
        writer,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    assert_eq!(commit["verification"], unsigned);
    let tree = commit["tree"]["sha"].as_str().unwrap();
    let person =
        json!({"name":"Signer","email":"signer@example.com","date":"2000-01-01T00:00:00Z"});
    let payload = format!("tree {tree}\nparent {sha}\nauthor Signer <signer@example.com> 946684800 +0000\ncommitter Signer <signer@example.com> 946684800 +0000\n\nSigned commit\n");
    let key: ssh_key::PrivateKey = ssh_key::private::Ed25519Keypair::from_seed(&[7; 32]).into();
    let signature = key
        .sign("git", ssh_key::HashAlg::Sha512, payload.as_bytes())
        .unwrap()
        .to_pem(ssh_key::LineEnding::LF)
        .unwrap();
    let signed = call(router, "POST", &format!("{base}/git/commits"), writer,
        json!({"message":"Signed commit\n","tree":tree,"parents":[sha],"author":person,"committer":person,"signature":signature}), ("git/create-commit", 201)).await;
    let verification = &signed["verification"];
    assert_eq!(verification["verified"], false);
    assert_eq!(verification["reason"], "unknown_key");
    assert_eq!(verification["payload"], payload);
    assert_eq!(
        verification["signature"].as_str().unwrap().trim_end(),
        signature.trim_end()
    );
    assert_eq!(verification["verified_at"], Value::Null);
    let signed_sha = signed["sha"].as_str().unwrap();
    for (path, operation, nested) in [
        ("git/commits", "git/get-commit", false),
        ("commits", "repos/get-commit", true),
    ] {
        let document = call(
            router,
            "GET",
            &format!("{base}/{path}/{signed_sha}"),
            writer,
            Value::Null,
            (operation, 200),
        )
        .await;
        assert_eq!(
            if nested {
                &document["commit"]["verification"]
            } else {
                &document["verification"]
            },
            verification
        );
    }
    let malformed = call(router, "POST", &format!("{base}/git/commits"), writer,
        json!({"message":"Signed commit\n","tree":tree,"parents":[sha],"author":person,"committer":person,"signature":"not a signature"}), ("git/create-commit", 201)).await;
    assert_eq!(malformed["verification"]["verified"], false);
    assert_eq!(malformed["verification"]["reason"], "malformed_signature");
    assert_eq!(malformed["verification"]["payload"], payload);

    let tag_payload = format!("object {sha}\ntype commit\ntag signed-fixture\ntagger Signer <signer@example.com> 946684800 +0000\n\nTag message\n");
    let signature = key
        .sign("git", ssh_key::HashAlg::Sha512, tag_payload.as_bytes())
        .unwrap()
        .to_pem(ssh_key::LineEnding::LF)
        .unwrap();
    let tag = call(router, "POST", &format!("{base}/git/tags"), writer,
        json!({"tag":"signed-fixture","message":format!("Tag message\n{signature}"),"object":sha,"type":"commit","tagger":person}), ("git/create-tag", 201)).await;
    assert_eq!(tag["verification"]["verified"], false);
    assert_eq!(tag["verification"]["reason"], "unknown_key");
    assert_eq!(tag["verification"]["payload"], tag_payload);
    assert_eq!(
        tag["verification"]["signature"]
            .as_str()
            .unwrap()
            .trim_end(),
        signature.trim_end()
    );
    let read_tag = call(
        router,
        "GET",
        &format!("{base}/git/tags/{}", tag["sha"].as_str().unwrap()),
        writer,
        Value::Null,
        ("git/get-tag", 200),
    )
    .await;
    assert_eq!(read_tag["verification"], tag["verification"]);
    let tag = call(router, "POST", &format!("{base}/git/tags"), writer,
        json!({"tag":"unsigned-fixture","message":"Unsigned tag","object":sha,"type":"commit","tagger":person}), ("git/create-tag", 201)).await;
    assert_eq!(tag["verification"], unsigned);

    // Once the key is registered, the signature is checked against the committer address.
    call(
        router,
        "POST",
        "/api/v3/user/ssh_signing_keys",
        writer,
        json!({"title":"Fixture","key":key.public_key().to_openssh().unwrap()}),
        ("users/create-ssh-signing-key-for-authenticated-user", 201),
    )
    .await;
    let foreign = call(
        router,
        "GET",
        &format!("{base}/git/commits/{signed_sha}"),
        writer,
        Value::Null,
        ("git/get-commit", 200),
    )
    .await;
    assert_eq!(foreign["verification"]["reason"], "no_user");
    let alice = json!({"name":"Alice","email":"alice@users.noreply","date":"2000-01-01T00:00:00Z"});
    let payload = |message: &str| {
        format!("tree {tree}\nparent {sha}\nauthor Alice <alice@users.noreply> 946684800 +0000\ncommitter Alice <alice@users.noreply> 946684800 +0000\n\n{message}\n")
    };
    let signed_by = |message: &str, signature: String| json!({"message":format!("{message}\n"),"tree":tree,"parents":[sha],"author":alice,"committer":alice,"signature":signature});
    let ssh = key
        .sign(
            "git",
            ssh_key::HashAlg::Sha512,
            payload("SSH signed").as_bytes(),
        )
        .unwrap()
        .to_pem(ssh_key::LineEnding::LF)
        .unwrap();
    let valid = call(
        router,
        "POST",
        &format!("{base}/git/commits"),
        writer,
        signed_by("SSH signed", ssh),
        ("git/create-commit", 201),
    )
    .await;
    assert_eq!(valid["verification"]["verified"], true, "{valid}");
    assert_eq!(valid["verification"]["reason"], "valid");
    assert!(valid["verification"]["verified_at"].is_string());

    let gpg = pgp::composed::SecretKeyParamsBuilder::default()
        .key_type(pgp::composed::KeyType::Ed25519Legacy)
        .can_certify(true)
        .can_sign(true)
        .primary_user_id("Alice <alice@users.noreply>".into())
        .build()
        .unwrap()
        .generate(rand::thread_rng())
        .unwrap();
    let gpg_sign = |text: &str| {
        pgp::composed::DetachedSignature::sign_binary_data(
            rand::thread_rng(),
            &gpg.primary_key,
            &pgp::types::Password::empty(),
            pgp::crypto::hash::HashAlgorithm::Sha256,
            text.as_bytes(),
        )
        .unwrap()
        .to_armored_string(Default::default())
        .unwrap()
    };
    let unknown = call(
        router,
        "POST",
        &format!("{base}/git/commits"),
        writer,
        signed_by("GPG signed", gpg_sign(&payload("GPG signed"))),
        ("git/create-commit", 201),
    )
    .await;
    assert_eq!(unknown["verification"]["reason"], "unknown_key");
    call(
        router,
        "POST",
        "/api/v3/user/gpg_keys",
        writer,
        json!({"armored_public_key":pgp::composed::SignedPublicKey::from(gpg.clone()).to_armored_string(Default::default()).unwrap()}),
        ("users/create-gpg-key-for-authenticated-user", 201),
    )
    .await;
    let valid = call(
        router,
        "GET",
        &format!("{base}/commits/{}", unknown["sha"].as_str().unwrap()),
        writer,
        Value::Null,
        ("repos/get-commit", 200),
    )
    .await;
    assert_eq!(valid["commit"]["verification"]["verified"], true, "{valid}");
    assert_eq!(valid["commit"]["verification"]["reason"], "valid");
    // A signature over other bytes does not verify, even from a registered key.
    let forged = call(
        router,
        "POST",
        &format!("{base}/git/commits"),
        writer,
        signed_by("GPG forged", gpg_sign(&payload("GPG signed"))),
        ("git/create-commit", 201),
    )
    .await;
    assert_eq!(forged["verification"]["verified"], false);
    assert_eq!(forged["verification"]["reason"], "invalid");
}
