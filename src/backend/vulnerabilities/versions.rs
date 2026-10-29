//! Ecosystem ordering for OSV interval endpoints, not dependency requirement resolution.
use std::cmp::Ordering;

fn semver(raw: &str) -> Option<semver::Version> {
    let raw = raw.trim_start_matches('v');
    let (release, suffix) = raw
        .find(['-', '+'])
        .map_or((raw, ""), |i| (&raw[..i], &raw[i..]));
    let missing = 3usize.checked_sub(release.split('.').count())?;
    let mut version =
        semver::Version::parse(&format!("{release}{}{suffix}", ".0".repeat(missing))).ok()?;
    version.build = semver::BuildMetadata::EMPTY;
    Some(version)
}
fn release(raw: &str) -> Option<Vec<u64>> {
    let mut parts = raw
        .split('.')
        .map(str::parse)
        .collect::<Result<Vec<u64>, _>>()
        .ok()?;
    while parts.last() == Some(&0) {
        parts.pop();
    }
    Some(parts)
}
fn numbers(a: &[u64], b: &[u64]) -> Ordering {
    (0..a.len().max(b.len()))
        .map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)))
        .find(|c| *c != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Python {
    epoch: u64,
    // Releases are compared separately with zero padding.
    pre: (i8, u64),
    post: (i8, u64),
    dev: (i8, u64),
    local: Option<Vec<Local>>,
}
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Local {
    Word(String),
    Number(u64),
}
fn python(raw: &str) -> Option<(Vec<u64>, Python)> {
    let raw = raw.trim_start_matches(['v', 'V']).to_ascii_lowercase();
    let (public, local) = raw.split_once('+').map_or((raw.as_str(), None), |(public, local)| (public, Some(local)));
    let local = match local {
        Some(local) => Some(local.split(['.', '-', '_']).map(|part| {
            if part.is_empty() { return None; }
            if part.bytes().all(|byte| byte.is_ascii_digit()) {
                Some(Local::Number(part.parse().ok()?))
            } else if part.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                Some(Local::Word(part.to_owned()))
            } else {
                None
            }
        }).collect::<Option<Vec<_>>>()?),
        None => None,
    };
    let (epoch, public) = match public.split_once('!') {
        Some((e, v)) => (e.parse().ok()?, v),
        None => (0, public),
    };
    let end = public
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(public.len());
    let (rel, mut tail) = public.split_at(end);
    let rel = release(rel.trim_end_matches('.'))?;
    let mut pre = None;
    let mut post = None;
    let mut dev = None;
    while !tail.is_empty() {
        tail = tail.trim_start_matches(['.', '-', '_']);
        let end = tail
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(tail.len());
        let (label, rest) = tail.split_at(end);
        let rest = rest.trim_start_matches(['.', '-', '_']);
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let number = if end == 0 {
            0
        } else {
            rest[..end].parse().ok()?
        };
        tail = &rest[end..];
        match label {
            "a" | "alpha" => pre = Some((0, number)),
            "b" | "beta" => pre = Some((1, number)),
            "c" | "rc" | "pre" | "preview" => pre = Some((2, number)),
            "post" | "rev" | "r" => post = Some((0, number)),
            "dev" => dev = Some((0, number)),
            "" if end > 0 => post = Some((0, number)),
            _ => return None,
        }
    }
    Some((
        rel,
        Python {
            epoch,
            pre: pre.unwrap_or(if dev.is_some() && post.is_none() {
                (-1, 0)
            } else {
                (3, 0)
            }),
            post: post.unwrap_or((-1, 0)),
            dev: dev.unwrap_or((1, 0)),
            local,
        },
    ))
}
#[derive(Debug, Eq, PartialEq)]
enum Token {
    Number(u64),
    Word(String),
}
fn tokens(raw: &str) -> Option<Vec<Token>> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = raw.as_bytes();
    for i in 0..=bytes.len() {
        let boundary = i == bytes.len()
            || !bytes[i].is_ascii_alphanumeric()
            || (i > start && bytes[i].is_ascii_digit() != bytes[start].is_ascii_digit());
        if boundary {
            if i > start {
                let s = &raw[start..i];
                out.push(if bytes[start].is_ascii_digit() {
                    Token::Number(s.parse().ok()?)
                } else {
                    Token::Word(s.to_owned())
                });
            }
            start = if i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i
            } else {
                i + 1
            };
        }
    }
    Some(out)
}
fn qualifier(s: &str) -> (i8, &str) {
    match s {
        "alpha" => (0, ""),
        "beta" => (1, ""),
        "milestone" => (2, ""),
        "rc" | "cr" => (3, ""),
        "snapshot" => (4, ""),
        "" | "final" | "ga" | "release" => (5, ""),
        "sp" => (6, ""),
        other => (7, other),
    }
}
/// One part of a Maven version as `ComparableVersion` parses it: `.` separates parts,
/// while `-` and a switch between digits and letters open a nested list, so `1.0-1`
/// sorts below `1.0.1`.
#[derive(Debug)]
enum MavenItem {
    Number(String),
    Word(String),
    List(Vec<MavenItem>),
}
fn maven_items(raw: &str) -> Option<Vec<MavenItem>> {
    fn part(digits: bool, s: &str) -> Option<MavenItem> {
        Some(if digits {
            MavenItem::Number(s.trim_start_matches('0').to_owned())
        } else {
            MavenItem::Word(s.to_owned())
        })
    }
    if raw.len() > crate::backend::db::dependency_inventory::MAX_VERSION_BYTES { return None; }
    let raw = raw.to_ascii_lowercase();
    let mut stack = vec![Vec::new()];
    let mut digits = false;
    let mut start = 0;
    for (i, c) in raw.char_indices() {
        if stack.len() > 32 { return None; }
        if c == '.' || c == '-' {
            if i > start && !digits && !stack.last()?.is_empty() {
                stack.push(Vec::new());
            }
            let item = if i == start {
                MavenItem::Number(String::new())
            } else {
                part(digits, &raw[start..i])?
            };
            stack.last_mut()?.push(item);
            start = i + 1;
            if c == '-' {
                stack.push(Vec::new());
            }
        } else {
            if c.is_ascii_digit() != digits && i > start {
                if !digits && !stack.last()?.is_empty() {
                    stack.push(Vec::new());
                }
                let word = match &raw[start..i] {
                    "a" if !digits => "alpha",
                    "b" if !digits => "beta",
                    "m" if !digits => "milestone",
                    other => other,
                };
                stack.last_mut()?.push(part(digits, word)?);
                start = i;
                stack.push(Vec::new());
            }
            digits = c.is_ascii_digit();
        }
    }
    if raw.len() > start {
        stack.last_mut()?.push(part(digits, &raw[start..])?);
    }
    while let Some(mut list) = stack.pop() {
        // Trailing zeros and release qualifiers carry no order: `1.0-final` is `1`.
        let mut i = list.len();
        while i > 0 {
            i -= 1;
            match &list[i] {
                item if maven_null(item) => {
                    list.remove(i);
                }
                MavenItem::List(_) => {}
                _ => break,
            }
        }
        match stack.last_mut() {
            Some(parent) => parent.push(MavenItem::List(list)),
            None => return Some(list),
        }
    }
    None
}
fn maven_null(item: &MavenItem) -> bool {
    match item {
        MavenItem::Number(n) => n.is_empty(),
        MavenItem::Word(w) => qualifier(w) == qualifier(""),
        MavenItem::List(l) => l.is_empty(),
    }
}
fn maven_item(a: Option<&MavenItem>, b: Option<&MavenItem>) -> Ordering {
    use MavenItem::{List, Number, Word};
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => maven_item(b, a).reverse(),
        (Some(Number(x)), None) => x.len().cmp(&0),
        (Some(Word(x)), None) => qualifier(x).cmp(&qualifier("")),
        (Some(List(x)), None) => x
            .iter()
            .map(|item| maven_item(Some(item), None))
            .find(|order| order.is_ne())
            .unwrap_or(Ordering::Equal),
        (Some(Number(x)), Some(Number(y))) => x.len().cmp(&y.len()).then_with(|| x.cmp(y)),
        (Some(Word(x)), Some(Word(y))) => qualifier(x).cmp(&qualifier(y)),
        (Some(List(x)), Some(List(y))) => maven_list(x, y),
        (Some(Number(_)), _) => Ordering::Greater,
        (_, Some(Number(_))) => Ordering::Less,
        (Some(List(_)), Some(Word(_))) => Ordering::Greater,
        (Some(Word(_)), Some(List(_))) => Ordering::Less,
    }
}
fn maven_list(a: &[MavenItem], b: &[MavenItem]) -> Ordering {
    (0..a.len().max(b.len()))
        .map(|i| maven_item(a.get(i), b.get(i)))
        .find(|order| order.is_ne())
        .unwrap_or(Ordering::Equal)
}
fn maven(a: &str, b: &str) -> Option<Ordering> {
    Some(maven_list(&maven_items(a)?, &maven_items(b)?))
}
fn gem(a: &str, b: &str) -> Option<Ordering> {
    let normalize = |raw: &str| -> Option<Vec<Token>> {
        let mut parts = tokens(&raw.replace('-', ".pre."))?;
        let release_end = parts.iter().position(|part| matches!(part, Token::Word(_))).unwrap_or(parts.len());
        let mut end = release_end;
        while end > 0 && parts[end - 1] == Token::Number(0) {
            parts.remove(end - 1);
            end -= 1;
        }
        Some(parts)
    };
    let a = normalize(a)?;
    let b = normalize(b)?;
    for i in 0..a.len().max(b.len()) {
        let zero = Token::Number(0);
        let order = match (a.get(i).unwrap_or(&zero), b.get(i).unwrap_or(&zero)) {
            (Token::Number(x), Token::Number(y)) => x.cmp(y),
            (Token::Word(x), Token::Word(y)) => x.cmp(y),
            (Token::Number(_), Token::Word(_)) => Ordering::Greater,
            (Token::Word(_), Token::Number(_)) => Ordering::Less,
        };
        if order != Ordering::Equal {
            return Some(order);
        }
    }
    Some(Ordering::Equal)
}
fn prerelease(a: &str, b: &str) -> Ordering {
    let a = a.split('.').collect::<Vec<_>>();
    let b = b.split('.').collect::<Vec<_>>();
    for (a, b) in a.iter().zip(&b) {
        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => a.cmp(b),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    a.len().cmp(&b.len())
}
pub fn compare(ecosystem: &str, a: &str, b: &str) -> Option<Ordering> {
    if a.len() > crate::backend::db::dependency_inventory::MAX_VERSION_BYTES || b.len() > crate::backend::db::dependency_inventory::MAX_VERSION_BYTES { return None; }
    match ecosystem {
        "PyPI" => {
            let (ar, av) = python(a)?;
            let (br, bv) = python(b)?;
            Some(
                av.epoch
                    .cmp(&bv.epoch)
                    .then_with(|| numbers(&ar, &br))
                    .then_with(|| av.pre.cmp(&bv.pre))
                    .then_with(|| av.post.cmp(&bv.post))
                    .then_with(|| av.dev.cmp(&bv.dev))
                    .then_with(|| av.local.cmp(&bv.local)),
            )
        }
        "Maven" => maven(a, b),
        "RubyGems" => gem(a, b),
        "NuGet" => {
            let a = a.split('+').next()?.to_ascii_lowercase();
            let b = b.split('+').next()?.to_ascii_lowercase();
            let parts = |v: &str| -> Option<(Vec<u64>, Option<String>)> {
                let (r, p) = v
                    .split_once('-')
                    .map_or((v, None), |(r, p)| (r, Some(p.to_owned())));
                Some((release(r)?, p))
            };
            let (ar, ap) = parts(&a)?;
            let (br, bp) = parts(&b)?;
            Some(numbers(&ar, &br).then_with(|| match (ap, bp) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => prerelease(&a, &b),
            }))
        }
        "Packagist" => composer(a, b),
        "Pub" => Some(semver(a)?.cmp(&semver(b)?).then_with(|| {
            match (a.split_once('+'), b.split_once('+')) {
                (Some((_, a)), Some((_, b))) => prerelease(a, b),
                (Some(_), None) => Ordering::Greater,
                (None, Some(_)) => Ordering::Less,
                _ => Ordering::Equal,
            }
        })),
        "npm" | "crates.io" | "Go" | "Hex" | "SEMVER" => Some(semver(a)?.cmp(&semver(b)?)),
        _ => None,
    }
}
fn composer(a: &str, b: &str) -> Option<Ordering> {
    fn parts(raw: &str) -> Option<(Vec<u64>, i8, u64)> {
        let raw = raw.trim_start_matches('v').split('+').next()?.to_ascii_lowercase();
        let end = raw.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(raw.len());
        let release = release(raw[..end].trim_end_matches('.'))?;
        let tail = raw[end..].trim_start_matches(['-', '.']);
        let end = tail.find(|c: char| c.is_ascii_digit()).unwrap_or(tail.len());
        let rank = match &tail[..end] {
            "dev" => 0, "alpha" | "a" => 1, "beta" | "b" => 2,
            "rc" => 3, "" | "stable" => 4, "pl" | "patch" | "p" => 5,
            _ => return None,
        };
        let number = if end == tail.len() { 0 } else { tail[end..].parse().ok()? };
        Some((release, rank, number))
    }
    let (ar, aq, an) = parts(a)?;
    let (br, bq, bn) = parts(b)?;
    Some(numbers(&ar, &br).then_with(|| aq.cmp(&bq)).then_with(|| an.cmp(&bn)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ecosystem_version_ordering() {
        for (e, a, b) in [
            ("npm", "1.0.0-rc.1", "1.0.0"),
            ("Go", "v1.2.3-0.20240101000000-abcdef", "v1.2.3"),
            ("PyPI", "1!1.0", "2.0"),
            ("PyPI", "1.0.dev1", "1.0a1"),
            ("PyPI", "1.0rc1", "1.0"),
            ("PyPI", "1.0", "1.0.post1"),
            ("Maven", "1.0-alpha1", "1.0-beta1"),
            ("Maven", "1.0-rc1", "1.0-SNAPSHOT"),
            ("Maven", "1.0-SNAPSHOT", "1.0"),
            ("Maven", "1.0", "1.0-sp1"),
            ("Maven", "1.0-1", "1.0.1"),
            ("Maven", "1.0", "1.0-1"),
            ("RubyGems", "1.0.pre", "1.0"),
            ("NuGet", "1.0.0-RC.2", "1.0.0-rc.10"),
            ("Packagist", "1.0.0-beta1", "1.0.0"),
        ] {
            let expected = if e == "PyPI" && a.starts_with("1!") {
                Ordering::Greater
            } else {
                Ordering::Less
            };
            assert_eq!(compare(e, a, b), Some(expected), "{e}: {a} vs {b}");
        }
        for (e, a, b) in [
            ("PyPI", "1.0.0", "1.0"),
            ("Maven", "1.0-final", "1.0"),
            ("NuGet", "1.0.0.0", "1.0.0"),
            ("RubyGems", "1.0.0", "1"),
        ] {
            assert_eq!(compare(e, a, b), Some(Ordering::Equal), "{e}: {a} vs {b}");
        }
    }
}
