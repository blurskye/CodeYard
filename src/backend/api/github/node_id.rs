//! Stable opaque identifiers shared by REST and GraphQL: a short type prefix (`R` repository,
//! `U` user, `I` issue, …; each kind's prefix is at its call site) and the database ID.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

/// A ref's identity is its repository and full name, not the commit it currently targets.
pub(crate) fn reference(repo_id: i64, name: &str) -> String {
    let bytes = [repo_id.to_be_bytes().as_slice(), name.as_bytes()].concat();
    format!("REF_{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn encode(kind: &str, id: i64) -> String {
    format!("{kind}_{}", URL_SAFE_NO_PAD.encode(id.to_be_bytes()))
}

pub(crate) fn decode(value: &str) -> Option<(&str, i64)> {
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

/// gh constructs comment IDs from MessagePack [0, repository ID, comment ID].
pub(crate) fn discussion_comment(repo: i64, comment: i64) -> String {
    let mut bytes = vec![0x93, 0, 0xcf];
    bytes.extend_from_slice(&repo.to_be_bytes());
    bytes.push(0xcf);
    bytes.extend_from_slice(&comment.to_be_bytes());
    format!("DC_{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn decode_discussion_comment(value: &str) -> Option<(i64, i64)> {
    let bytes = URL_SAFE_NO_PAD.decode(value.strip_prefix("DC_")?).ok()?;
    let mut bytes = bytes.as_slice();
    if bytes.first() != Some(&0x93) {
        return None;
    }
    bytes = &bytes[1..];
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
    let repo = integer(&mut bytes)?;
    let comment = integer(&mut bytes)?;
    (bytes.is_empty() && repo > 0 && comment > 0).then_some((repo, comment))
}

/// Review requests use their actual composite primary key, not a synthesized database row ID.
pub(crate) fn review_request(pull: i64, team: bool, target: i64) -> String {
    let bytes = [
        pull.to_be_bytes().as_slice(),
        &[u8::from(team)],
        target.to_be_bytes().as_slice(),
    ]
    .concat();
    format!("RR_{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn decode_review_request(value: &str) -> Option<(i64, bool, i64)> {
    let bytes = URL_SAFE_NO_PAD.decode(value.strip_prefix("RR_")?).ok()?;
    if bytes.len() != 17 || bytes[8] > 1 {
        return None;
    }
    let pull = i64::from_be_bytes(bytes[..8].try_into().ok()?);
    let target = i64::from_be_bytes(bytes[9..].try_into().ok()?);
    (pull > 0 && target > 0).then_some((pull, bytes[8] != 0, target))
}

/// Git object fields such as comments, checks and URLs belong to one immutable repository ID.
pub(crate) fn git_object(kind: &str, repo: i64, oid: &str) -> String {
    git(kind, &format!("{repo}/{}", oid.to_ascii_lowercase()))
}

/// Non-numeric keys for gists, licenses and composite resources.
pub(crate) fn git(kind: &str, key: &str) -> String {
    format!("{kind}_{}", URL_SAFE_NO_PAD.encode(key))
}

#[cfg(test)]
mod tests {
    use super::{decode, decode_review_request, encode, review_request};

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
    fn malformed_node_ids_are_refused() {
        for value in ["", "R_", "R_!", "R_AQ", "X_AAAAAAAAAAE", "R_AAAAAAAAAAA"] {
            assert_eq!(decode(value), None);
        }
        assert_eq!(decode(&encode("R", -1)), None);
        assert_eq!(decode(&super::git("T", &"a".repeat(40))), None);
    }
}
