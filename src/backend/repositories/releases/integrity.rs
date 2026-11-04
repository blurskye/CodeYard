//! A signed release statement binds the tag, commit and asset digests, not editable notes.
use crate::backend::cluster::NodeKey;
use anyhow::{ensure, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, str::FromStr};
const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
const PREDICATE_TYPE: &str = "urn:codeyard:release:v1";
const MAX_PAYLOAD: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub name: String,
    pub size: i64,
    pub sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    pub name: String,
    pub digest: BTreeMap<String, String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Predicate {
    pub repository_id: i64,
    pub repository: String,
    pub tag: String,
    pub tag_oid: String,
    pub commit_sha: String,
    pub assets: Vec<Artifact>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    #[serde(rename = "_type")]
    statement_type: String,
    pub subject: Vec<Subject>,
    #[serde(rename = "predicateType")]
    predicate_type: String,
    pub predicate: Predicate,
}
fn hex_digest(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
impl Statement {
    pub fn new(
        repository_id: i64,
        repository: &str,
        tag: &str,
        tag_oid: &str,
        commit_sha: &str,
        mut assets: Vec<Artifact>,
    ) -> Result<Self> {
        assets.sort_by(|a, b| a.name.cmp(&b.name));
        let mut subject = vec![Subject {
            name: format!("git+{repository}@refs/tags/{tag}"),
            digest: BTreeMap::from([("sha1".into(), commit_sha.to_owned())]),
        }];
        subject.extend(assets.iter().map(|asset| Subject {
            name: asset.name.clone(),
            digest: BTreeMap::from([("sha256".into(), asset.sha256.clone())]),
        }));
        let statement = Self {
            statement_type: STATEMENT_TYPE.into(),
            predicate_type: PREDICATE_TYPE.into(),
            subject,
            predicate: Predicate {
                repository_id,
                repository: repository.into(),
                tag: tag.into(),
                tag_oid: tag_oid.into(),
                commit_sha: commit_sha.into(),
                assets,
            },
        };
        statement.validate()?;
        Ok(statement)
    }
    pub fn validate(&self) -> Result<()> {
        let predicate = &self.predicate;
        ensure!(
            self.statement_type == STATEMENT_TYPE && self.predicate_type == PREDICATE_TYPE,
            "Unsupported release statement"
        );
        let repository = url::Url::parse(&predicate.repository)?;
        ensure!(
            matches!(repository.scheme(), "http" | "https")
                && repository.host_str().is_some()
                && repository.username().is_empty()
                && repository.password().is_none()
                && repository.query().is_none()
                && repository.fragment().is_none(),
            "Invalid release repository URL"
        );
        ensure!(
            predicate.repository_id > 0
                && predicate.tag.len() <= 255
                && codeyard_git::tag_ref(&predicate.tag).is_some(),
            "Invalid release identity"
        );
        ensure!(
            hex_digest(&predicate.tag_oid, 40) && hex_digest(&predicate.commit_sha, 40),
            "Invalid pinned release objects"
        );
        ensure!(
            predicate.assets.len() <= 1000,
            "A release can contain at most 1000 assets"
        );
        let mut previous: Option<&str> = None;
        for asset in &predicate.assets {
            ensure!(
                !asset.name.is_empty()
                    && asset.name.len() <= 255
                    && !matches!(asset.name.as_str(), "." | "..")
                    && !asset.name.contains(['/', '\\'])
                    && !asset.name.chars().any(char::is_control),
                "Invalid release asset name"
            );
            ensure!(
                previous.is_none_or(|name| name < asset.name.as_str()),
                "Release assets must be uniquely ordered"
            );
            previous = Some(&asset.name);
            ensure!(
                asset.size > 0
                    && asset.size <= super::MAX_ASSET_BYTES as i64
                    && hex_digest(&asset.sha256, 64),
                "Invalid release asset digest"
            );
        }
        let mut subjects = vec![Subject {
            name: format!("git+{}@refs/tags/{}", predicate.repository, predicate.tag),
            digest: BTreeMap::from([("sha1".into(), predicate.commit_sha.clone())]),
        }];
        subjects.extend(predicate.assets.iter().map(|asset| Subject {
            name: asset.name.clone(),
            digest: BTreeMap::from([("sha256".into(), asset.sha256.clone())]),
        }));
        ensure!(
            self.subject == subjects,
            "Release subjects do not match the published objects"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signature {
    pub keyid: String,
    pub sig: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    #[serde(rename = "payloadType")]
    pub payload_type: String,
    pub payload: String,
    pub signatures: Vec<Signature>,
}
/// DSSE's length-prefixed encoding prevents cross-protocol signing ambiguities.
fn signing_bytes(payload: &[u8]) -> Vec<u8> {
    let mut encoded = format!(
        "DSSEv1 {} {} {} ",
        PAYLOAD_TYPE.len(),
        PAYLOAD_TYPE,
        payload.len()
    )
    .into_bytes();
    encoded.extend_from_slice(payload);
    encoded
}
impl Attestation {
    pub fn sign(statement: &Statement, key: &NodeKey) -> Result<Self> {
        statement.validate()?;
        let payload = serde_json::to_vec(statement)?;
        ensure!(
            payload.len() <= MAX_PAYLOAD,
            "Release statement is too large"
        );
        let signature = key.sign(&signing_bytes(&payload));
        Ok(Self {
            payload_type: PAYLOAD_TYPE.into(),
            payload: STANDARD.encode(payload),
            signatures: vec![Signature {
                keyid: key.public_hex(),
                sig: STANDARD.encode(signature),
            }],
        })
    }
    /// The caller pins this public key to the forge/node it trusts. No GitHub/Sigstore
    /// identity or transparency-log claim is made by a self-hosted node signature.
    pub fn verify(&self, expected_key: &str) -> Result<Statement> {
        ensure!(
            self.payload_type == PAYLOAD_TYPE && self.signatures.len() == 1,
            "Unsupported release attestation"
        );
        ensure!(
            self.payload.len() <= MAX_PAYLOAD * 4 / 3 + 4,
            "Release statement is too large"
        );
        let signature = &self.signatures[0];
        ensure!(
            signature.keyid == expected_key,
            "Release signer does not match the trusted node key"
        );
        let public = iroh::PublicKey::from_str(expected_key)?;
        let bytes = STANDARD.decode(&self.payload)?;
        ensure!(bytes.len() <= MAX_PAYLOAD, "Release statement is too large");
        let signature = STANDARD.decode(&signature.sig)?;
        let signature = signature.as_slice().try_into()?;
        public.verify(&signing_bytes(&bytes), &signature)?;
        let statement: Statement = serde_json::from_slice(&bytes)?;
        statement.validate()?;
        Ok(statement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signed_releases_bind_objects_and_require_the_trusted_node_key() {
        let key = NodeKey::generate();
        let other = NodeKey::generate();
        let statement = Statement::new(
            1,
            "https://forge.example/alice/proj",
            "v1",
            &"a".repeat(40),
            &"b".repeat(40),
            vec![Artifact {
                name: "binary.zip".into(),
                size: 100,
                sha256: "c".repeat(64),
            }],
        )
        .unwrap();
        let signed = Attestation::sign(&statement, &key).unwrap();
        assert_eq!(signed.verify(&key.public_hex()).unwrap(), statement);
        assert!(signed.verify(&other.public_hex()).is_err());
        let mut tampered = signed.clone();
        let mut changed = statement.clone();
        changed.predicate.assets[0].size += 1;
        tampered.payload = STANDARD.encode(serde_json::to_vec(&changed).unwrap());
        assert!(tampered.verify(&key.public_hex()).is_err());
        let mut tampered = signed.clone();
        tampered.payload_type = "application/json".into();
        assert!(tampered.verify(&key.public_hex()).is_err());
        let mut tampered = signed;
        tampered.signatures[0].sig = STANDARD.encode([0_u8; 64]);
        assert!(tampered.verify(&key.public_hex()).is_err());
    }
}
