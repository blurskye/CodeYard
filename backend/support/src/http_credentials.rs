//! Bounded Authorization header decoding; each adapter owns the meaning of the credential.
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use http::header;
use http::HeaderMap;

pub fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") || encoded.len() > 2048 {
        return None;
    }
    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_owned(), password.to_owned()))
}

/// One bearer credential, as the Actions runtime HTTP protocols send it.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.split_once(' ')?;
    let base = token.trim_end_matches('=');
    (scheme.eq_ignore_ascii_case("bearer")
        && token.len() <= 4096
        && !base.is_empty()
        && base
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/".contains(&byte)))
    .then_some(token)
}

pub fn token(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.split_once(' ')?;
    if !(scheme.eq_ignore_ascii_case("token") || scheme.eq_ignore_ascii_case("bearer"))
        || token.is_empty()
        || token.len() > 2048
        || token.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    Some(token)
}

pub fn cookie(headers: &HeaderMap, name: &str, limit: usize) -> Option<String> {
    let mut found = None;
    for cookies in headers.get_all(header::COOKIE).iter() {
        for cookie in cookies.to_str().ok()?.split(';') {
            let Some((key, value)) = cookie.trim().split_once('=') else {
                continue;
            };
            if key != name {
                continue;
            }
            if found.is_some() || value.len() > limit {
                return None;
            }
            found = Some(value);
        }
    }
    found.map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_credentials_and_cookie_values_are_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer a".parse().unwrap());
        assert_eq!(token(&headers), Some("a"));
        headers.append(header::AUTHORIZATION, "Bearer b".parse().unwrap());
        assert!(token(&headers).is_none());
        assert!(bearer(&headers).is_none());
        assert!(basic(&headers).is_none());
        headers.insert(
            header::COOKIE,
            "session_id=a; other=b; session_id=c".parse().unwrap(),
        );
        assert!(cookie(&headers, "session_id", 70).is_none());
        headers.insert(header::COOKIE, "session_id=aaa".parse().unwrap());
        assert!(cookie(&headers, "session_id", 2).is_none());
    }
}
