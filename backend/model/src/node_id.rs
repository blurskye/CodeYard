//! Stable opaque identifiers shared by REST and GraphQL: a short type prefix (`R` repository,
//! `U` user, `I` issue, …; each kind's prefix is at its call site) and the database ID.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

/// A ref's identity is its repository and full name, not the commit it currently targets.
pub fn reference(repo_id: i64, name: &str) -> String {
    let bytes = [repo_id.to_be_bytes().as_slice(), name.as_bytes()].concat();
    format!("REF_{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn encode(kind: &str, id: i64) -> String {
    format!("{kind}_{}", URL_SAFE_NO_PAD.encode(id.to_be_bytes()))
}

pub fn decode(value: &str) -> Option<(&str, i64)> {
    let (kind, value) = value.split_once('_')?;
    if !matches!(
        kind,
        "R" | "D"
            | "DIC"
            | "I"
            | "PR"
            | "U"
            | "O"
            | "T"
            | "L"
            | "M"
            | "IT"
            | "LB"
            | "RRS"
            | "IC"
            | "PC"
            | "REA"
            | "RE"
            | "RA"
            | "CS"
            | "CRU"
            | "CSU"
            | "PRR"
            | "PRRC"
            | "IE"
            | "S"
            | "ART"
            | "CC"
            | "OI"
            | "EN"
            | "GBP"
            | "PVT"
            | "PVTF"
            | "PVTSSF"
            | "PVTIF"
            | "PVTI"
            | "PVTV"
            | "DI"
            | "DE"
            | "DES"
            | "DPR"
    ) {
        return None;
    }
    let bytes: [u8; 8] = URL_SAFE_NO_PAD.decode(value).ok()?.try_into().ok()?;
    let id = i64::from_be_bytes(bytes);
    (id > 0).then_some((kind, id))
}

/// Root-bearing IDs route through one account or repository after it moves.
/// The simple root IDs and the former byte form still resolve as documented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Routed<'a> {
    pub kind: &'a str,
    pub root: i64,
    pub id: i64,
}

fn messagepack(root: i64, id: i64) -> Vec<u8> {
    let mut bytes = vec![0x93, 0, 0xcf];
    bytes.extend_from_slice(&root.to_be_bytes());
    bytes.push(0xcf);
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes
}

/// Issue a [0, root ID, ID] value. A caller supplies its repository or owner root,
/// never an unrelated row ID or an inferred allocation prefix.
pub fn scoped(kind: &str, root: i64, id: i64) -> Option<String> {
    codeyard_support::cells::Prefix::for_id(root).ok()?;
    codeyard_support::cells::Prefix::for_id(id).ok()?;
    if kind.is_empty() || kind.len() > 8 || !kind.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return None;
    }
    Some(format!(
        "{kind}_{}",
        URL_SAFE_NO_PAD.encode(messagepack(root, id))
    ))
}

fn decode_messagepack(bytes: &[u8]) -> Option<(i64, i64)> {
    let mut bytes = bytes.strip_prefix(&[0x93])?;
    fn integer(bytes: &mut &[u8]) -> Option<i64> {
        let (&tag, rest) = bytes.split_first()?;
        *bytes = rest;
        let size = match tag {
            0..=127 => return Some(i64::from(tag)),
            0xcc => 1,
            0xcd => 2,
            0xce => 4,
            0xcf => 8,
            _ => return None,
        };
        let number = bytes
            .get(..size)?
            .iter()
            .fold(0u64, |n, b| (n << 8) | u64::from(*b));
        *bytes = &bytes[size..];
        i64::try_from(number).ok()
    }
    if integer(&mut bytes)? != 0 {
        return None;
    }
    let root = integer(&mut bytes)?;
    let id = integer(&mut bytes)?;
    (bytes.is_empty() && root > 0 && id > 0).then_some((root, id))
}

/// Decode before routing without querying an object's cell. The resource cell still
/// checks that the ID belongs to this root; routing hints never grant access.
pub fn routed(value: &str) -> Option<Routed<'_>> {
    if value.len() > 128 {
        return None;
    }
    let (kind, encoded) = value.split_once('_')?;
    if kind.is_empty() || kind.len() > 8 || !kind.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    if let Some((root, id)) = decode_messagepack(&bytes) {
        codeyard_support::cells::Prefix::for_id(root).ok()?;
        codeyard_support::cells::Prefix::for_id(id).ok()?;
        return Some(Routed { kind, root, id });
    }
    let (kind, id) = decode(value)?;
    Some(Routed { kind, root: id, id })
}

/// gh constructs comment IDs from MessagePack [0, repository ID, comment ID].
pub fn discussion_comment(repo: i64, comment: i64) -> String {
    format!("DC_{}", URL_SAFE_NO_PAD.encode(messagepack(repo, comment)))
}

pub fn decode_discussion_comment(value: &str) -> Option<(i64, i64)> {
    if value.len() > 128 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(value.strip_prefix("DC_")?).ok()?;
    decode_messagepack(&bytes)
}

/// Review requests use their actual composite primary key, not a synthesized database row ID.
pub fn review_request(pull: i64, team: bool, target: i64) -> String {
    let bytes = [
        pull.to_be_bytes().as_slice(),
        &[u8::from(team)],
        target.to_be_bytes().as_slice(),
    ]
    .concat();
    format!("RR_{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn decode_review_request(value: &str) -> Option<(i64, bool, i64)> {
    let bytes = URL_SAFE_NO_PAD.decode(value.strip_prefix("RR_")?).ok()?;
    if bytes.len() != 17 || bytes[8] > 1 {
        return None;
    }
    let pull = i64::from_be_bytes(bytes[..8].try_into().ok()?);
    let target = i64::from_be_bytes(bytes[9..].try_into().ok()?);
    (pull > 0 && target > 0).then_some((pull, bytes[8] != 0, target))
}

/// Git object fields such as comments, checks and URLs belong to one immutable repository ID.
pub fn git_object(kind: &str, repo: i64, oid: &str) -> String {
    git(kind, &format!("{repo}/{}", oid.to_ascii_lowercase()))
}

/// Non-numeric keys for gists, licenses and composite resources.
pub fn git(kind: &str, key: &str) -> String {
    format!("{kind}_{}", URL_SAFE_NO_PAD.encode(key))
}

#[cfg(test)]
mod tests {
    use crate::node_id::decode;
    use crate::node_id::decode_review_request;
    use crate::node_id::encode;
    use crate::node_id::review_request;
    use base64::Engine;

    #[test]
    fn node_ids_round_trip_without_changing_rest_ids() {
        assert_eq!(encode("R", 1), "R_AAAAAAAAAAE");
        for kind in [
            "R", "D", "DIC", "I", "PR", "U", "O", "L", "IT", "IC", "PC", "CC", "T", "M", "REA",
            "RE", "RA", "CS", "PRR", "PRRC", "IE", "S", "ART", "OI", "EN", "GBP", "PVT", "PVTF",
            "PVTSSF", "PVTIF", "PVTI", "PVTV", "DI", "DE", "DES", "DPR",
        ] {
            for id in [1, i64::MAX] {
                assert_eq!(decode(&encode(kind, id)), Some((kind, id)));
            }
        }
    }

    #[test]
    fn review_request_ids_retain_both_native_keys() {
        for team in [false, true] {
            for (pull, target) in [(1, 2), (i64::MAX, 1)] {
                assert_eq!(
                    decode_review_request(&review_request(pull, team, target)),
                    Some((pull, team, target))
                );
            }
        }
        for value in ["RR_", "RR_!", "R_AAAAAAAAAAE"] {
            assert_eq!(decode_review_request(value), None);
        }
        assert_eq!(decode_review_request(&review_request(0, false, 1)), None);
        assert_eq!(decode_review_request(&review_request(1, true, -1)), None);
    }

    #[test]
    fn routed_nodes_keep_the_root_when_object_and_root_origins_differ() {
        use super::{routed, scoped, Routed};
        let root = *codeyard_support::cells::Prefix::new(1).ids().start();
        let id = *codeyard_support::cells::Prefix::new(2).ids().start();
        let value = scoped("IC", root, id).unwrap();
        assert_eq!(
            routed(&value),
            Some(Routed {
                kind: "IC",
                root,
                id
            })
        );
        assert_eq!(
            routed(&encode("U", root)),
            Some(Routed {
                kind: "U",
                root,
                id: root
            })
        );
        assert_eq!(
            super::decode_discussion_comment(&super::discussion_comment(root, id)),
            Some((root, id))
        );
        assert!(scoped("IC", 0, id).is_none());
        assert!(scoped("IC", root, codeyard_support::cells::MAX_ID + 1).is_none());
        for raw in [
            vec![0x93, 0, 1, 2, 3],
            vec![0x93, 1, 1, 2],
            vec![0x93, 0, 0, 2],
            vec![0x93, 0, 0xd3, 0, 0, 0, 0, 0, 0, 0, 1, 2],
        ] {
            assert!(routed(&format!("IC_{}", super::URL_SAFE_NO_PAD.encode(raw))).is_none());
        }
    }

    #[test]
    fn malformed_node_ids_are_refused() {
        for value in ["", "R_", "R_!", "R_AQ", "X_AAAAAAAAAAE", "R_AAAAAAAAAAA"] {
            assert_eq!(decode(value), None);
        }
        assert_eq!(decode(&encode("R", -1)), None);
        assert_eq!(decode(&crate::node_id::git("T", &"a".repeat(40))), None);
    }
}
