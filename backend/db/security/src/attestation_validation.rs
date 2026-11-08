use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use codeyard_database::native_operation;
use codeyard_database::DbPool;
use serde_json::Value;

fn valid_verification_material(material: &Value) -> bool {
    let certificate = |value: &Value| {
        value
            .get("rawBytes")
            .and_then(Value::as_str)
            .and_then(|encoded| STANDARD.decode(encoded).ok())
            .is_some_and(|bytes| !bytes.is_empty())
    };
    match (
        material.get("publicKey"),
        material.get("certificate"),
        material.get("x509CertificateChain"),
    ) {
        (Some(key), None, None) => key
            .get("hint")
            .and_then(Value::as_str)
            .is_some_and(|hint| !hint.is_empty()),
        (None, Some(value), None) => certificate(value),
        (None, None, Some(chain)) => chain
            .get("certificates")
            .and_then(Value::as_array)
            .is_some_and(|certificates| {
                !certificates.is_empty() && certificates.iter().all(certificate)
            }),
        _ => false,
    }
}

pub struct PreparedBundle {
    pub bundle: String,
    pub predicate_type: String,
    pub subjects: Vec<String>,
}
impl PreparedBundle {
    /// The bundle's sha256 subject digests, lowercase and without repeats.
    pub fn subjects(&self) -> &[String] {
        &self.subjects
    }

    pub fn new(bundle: &Value) -> Result<Self, AttestationError> {
        let text = bundle.to_string();
        if text.len() > crate::attestations::MAX_BUNDLE_BYTES {
            return Err(AttestationError::Size);
        }
        let invalid = || AttestationError::InvalidBundle;
        let media = bundle
            .get("mediaType")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        if !matches!(
            media,
            "application/vnd.dev.sigstore.bundle.v0.1+json"
                | "application/vnd.dev.sigstore.bundle.v0.2+json"
                | "application/vnd.dev.sigstore.bundle.v0.3+json"
                | "application/vnd.dev.sigstore.bundle.v0.4+json"
                | "application/vnd.dev.sigstore.bundle+json;version=0.1"
                | "application/vnd.dev.sigstore.bundle+json;version=0.2"
        ) || !bundle
            .get("verificationMaterial")
            .is_some_and(valid_verification_material)
        {
            return Err(invalid());
        }
        let envelope = bundle.get("dsseEnvelope").ok_or_else(invalid)?;
        if envelope.get("payloadType").and_then(Value::as_str)
            != Some("application/vnd.in-toto+json")
        {
            return Err(invalid());
        }
        let signatures = envelope
            .get("signatures")
            .and_then(Value::as_array)
            .filter(|signatures| !signatures.is_empty())
            .ok_or_else(invalid)?;
        for signature in signatures {
            let signature = signature
                .get("sig")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            if STANDARD
                .decode(signature)
                .map_err(|_| invalid())?
                .is_empty()
            {
                return Err(invalid());
            }
        }
        let payload = STANDARD
            .decode(
                envelope
                    .get("payload")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?,
            )
            .map_err(|_| invalid())?;
        let statement: Value = serde_json::from_slice(&payload).map_err(|_| invalid())?;
        if !matches!(
            statement.get("_type").and_then(Value::as_str),
            Some("https://in-toto.io/Statement/v1" | "https://in-toto.io/Statement/v0.1")
        ) || !statement.get("predicate").is_some_and(Value::is_object)
        {
            return Err(invalid());
        }
        let predicate_type = statement
            .get("predicateType")
            .and_then(Value::as_str)
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 2048
                    && !value.chars().any(char::is_whitespace)
                    && !value.chars().any(char::is_control)
                    && url::Url::parse(value).is_ok()
            })
            .ok_or_else(invalid)?
            .to_owned();
        let subjects = statement
            .get("subject")
            .and_then(Value::as_array)
            .filter(|subjects| !subjects.is_empty() && subjects.len() <= 1000)
            .ok_or_else(invalid)?;
        let mut digests = std::collections::BTreeSet::new();
        for subject in subjects {
            let digest = subject
                .get("digest")
                .and_then(Value::as_object)
                .ok_or_else(invalid)?;
            let mut recognized = false;
            for algorithm in ["sha256", "sha512"] {
                if let Some(hex) = digest.get(algorithm) {
                    let hex = hex.as_str().ok_or_else(invalid)?;
                    digests.insert(
                        subject_digest(&format!("{algorithm}:{hex}")).map_err(|_| invalid())?,
                    );
                    recognized = true;
                }
            }
            if !recognized {
                return Err(invalid());
            }
        }
        Ok(Self {
            bundle: text,
            predicate_type,
            subjects: digests.into_iter().collect(),
        })
    }
}

pub async fn create(
    pool: &DbPool,
    actor: impl Into<codeyard_database::queries::credentials::Actor>,
    repo: i64,
    bundle: &PreparedBundle,
) -> Result<i64, AttestationError> {
    use sha2::{Digest, Sha256};
    let actor = actor.into();
    let bundle = crate::attestations::Bundle {
        hash: format!("{:x}", Sha256::digest(bundle.bundle.as_bytes())),
        json: bundle.bundle.clone(),
        predicate: bundle.predicate_type.clone(),
        subjects: bundle.subjects.iter().cloned().collect(),
    };
    let user = codeyard_db_identity::users::accounts::get_user_by_id(pool, actor.user_id)
        .await
        .map_err(AttestationError::Authority)?
        .ok_or(AttestationError::Denied)?;
    crate::attestations::create(
        pool,
        actor,
        repo,
        &bundle,
        if user.username == codeyard_database::queries::ci_tokens::BOT {
            "workflow"
        } else {
            "user"
        },
    )
    .await
    .map_err(
        |error| match error.downcast_ref::<crate::attestations::Refusal>() {
            Some(crate::attestations::Refusal::Denied) => AttestationError::Denied,
            Some(crate::attestations::Refusal::Missing) => AttestationError::NotFound,
            _ => AttestationError::Authority(error),
        },
    )
}
native_operation!(codeyard_database::queries::postgres::repository_attestations::list,
    codeyard_database::queries::sqlite::repository_attestations::list,
    fn list(pool: &DbPool, actor: Option<i64>, repo: i64, digest: &str, page: &Page<'_>) -> Result<(Vec<Attestation>, bool), AttestationError>);
native_operation!(codeyard_database::queries::postgres::repository_attestations::get,
    codeyard_database::queries::sqlite::repository_attestations::get,
    fn get(pool: &DbPool, actor: Option<i64>, repo: i64, id: i64) -> Result<Attestation, AttestationError>);

use codeyard_database::queries::attestation_validation::Attestation;

use codeyard_database::queries::attestation_validation::subject_digest;

use codeyard_database::queries::attestation_validation::Page;

use codeyard_database::queries::attestation_validation::AttestationError;

#[cfg(test)]
mod tests {
    use super::*;

    fn statement() -> Value {
        serde_json::json!({"_type":"https://in-toto.io/Statement/v1",
            "predicateType":"https://slsa.dev/provenance/v1", "predicate":{},
            "subject":[{"name":"one","digest":{"sha256":"A".repeat(64)}},
                       {"name":"two","digest":{"sha256":"a".repeat(64)}}]})
    }
    fn bundle(statement: &Value) -> Value {
        // This unit exercises format validation, not a cryptographic trust verdict.
        serde_json::json!({"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json",
            "verificationMaterial":{"publicKey":{"hint":"fixture"}},
            "dsseEnvelope":{"payloadType":"application/vnd.in-toto+json",
                "payload":STANDARD.encode(statement.to_string()),
                "signatures":[{"sig":"AQ=="}]}})
    }
    #[test]
    fn preparation_preserves_evidence_and_normalizes_subject_indexes() {
        let input = bundle(&statement());
        let prepared = PreparedBundle::new(&input).unwrap();
        assert_eq!(prepared.bundle, input.to_string());
        assert_eq!(
            prepared.subjects,
            vec![format!("sha256:{}", "a".repeat(64))]
        );
    }
    #[test]
    fn preparation_rejects_malformed_statements_and_signatures() {
        for field in ["_type", "predicateType", "predicate", "subject"] {
            let mut invalid = statement();
            invalid.as_object_mut().unwrap().remove(field);
            assert!(matches!(
                PreparedBundle::new(&bundle(&invalid)),
                Err(AttestationError::InvalidBundle)
            ));
        }
        for invalid in [serde_json::json!(null), serde_json::json!("not an object")] {
            let mut input = statement();
            input["predicate"] = invalid;
            assert!(PreparedBundle::new(&bundle(&input)).is_err());
        }
        for predicate in [
            "not a URI",
            "https://example.test/with a space",
            "https://example.test/\n",
        ] {
            let mut input = statement();
            input["predicateType"] = serde_json::json!(predicate);
            assert!(PreparedBundle::new(&bundle(&input)).is_err());
        }
        for material in [
            serde_json::json!({}),
            serde_json::json!({"publicKey":{}}),
            serde_json::json!({"certificate":{"rawBytes":""}}),
            serde_json::json!({"x509CertificateChain":{"certificates":[]}}),
            serde_json::json!({"publicKey":{"hint":"fixture"},"certificate":{"rawBytes":"AQ=="}}),
        ] {
            let mut input = bundle(&statement());
            input["verificationMaterial"] = material;
            assert!(PreparedBundle::new(&input).is_err());
        }
        for signature in ["", "invalid base64"] {
            let mut input = bundle(&statement());
            input["dsseEnvelope"]["signatures"][0]["sig"] = serde_json::json!(signature);
            assert!(PreparedBundle::new(&input).is_err());
        }
        let mut input = bundle(&statement());
        input["extra"] = serde_json::json!("x".repeat(1024 * 1024));
        assert!(matches!(
            PreparedBundle::new(&input),
            Err(AttestationError::Size)
        ));
    }
    #[test]
    fn digest_and_cursor_bounds_are_authoritative() {
        for invalid in [
            "a".repeat(64),
            format!("sha512:{}", "a".repeat(64)),
            format!("sha256:{}", "a".repeat(63)),
            format!("sha256:{}g", "a".repeat(63)),
        ] {
            assert!(subject_digest(&invalid).is_err());
        }
        assert_eq!(
            subject_digest(&format!("sha256:{}", "A".repeat(64))).unwrap(),
            format!("sha256:{}", "a".repeat(64))
        );
        for page in [
            Page {
                after: Some(1),
                before: Some(2),
                limit: 1,
                predicate_type: None,
            },
            Page {
                after: Some(0),
                before: None,
                limit: 1,
                predicate_type: None,
            },
            Page {
                after: None,
                before: None,
                limit: 101,
                predicate_type: None,
            },
        ] {
            assert!(page.validate().is_err());
        }
    }
}
