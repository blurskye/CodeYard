//! Reversible Git path identities shared by storage, SSR and hydrated links.
//! UTF-8 paths stay unchanged. An invalid-UTF-8 component uses NUL plus hex internally;
//! NUL cannot occur in a Git filename, so display labels can never alias that identity.
const HEX: &[u8] = b"0123456789ABCDEF";

pub fn from_bytes(path: &[u8]) -> String {
    path.split(|byte| *byte == b'/').map(|part| {
        match std::str::from_utf8(part) {
            Ok(text) if !part.contains(&0) => text.to_owned(),
            _ => {
                let mut text = String::from("\0");
                for byte in part { text.push(HEX[(byte >> 4) as usize] as char); text.push(HEX[(byte & 15) as usize] as char); }
                text
            }
        }
    }).collect::<Vec<_>>().join("/")
}

fn nibble(byte: u8) -> Option<u8> {
    match byte { b'0'..=b'9' => Some(byte - b'0'), b'a'..=b'f' => Some(byte - b'a' + 10), b'A'..=b'F' => Some(byte - b'A' + 10), _ => None }
}

pub fn bytes(path: &str) -> Vec<u8> {
    let mut raw = Vec::new();
    for (index, part) in path.split('/').enumerate() {
        if index != 0 { raw.push(b'/'); }
        if let Some(hex) = part.strip_prefix('\0') {
            if hex.len() % 2 == 0 && hex.bytes().all(|byte| nibble(byte).is_some()) {
                raw.extend(hex.as_bytes().chunks_exact(2).map(|pair| nibble(pair[0]).unwrap() * 16 + nibble(pair[1]).unwrap()));
                continue;
            }
        }
        raw.extend_from_slice(part.as_bytes());
    }
    raw
}

pub fn display(path: &str) -> String { String::from_utf8_lossy(&bytes(path)).into_owned() }

/// Encode original bytes, not replacement characters. Only actual separators remain slashes.
pub fn url(path: &str) -> String {
    let mut result = String::new();
    for byte in bytes(path) {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) { result.push(byte as char); }
        else { result.push('%'); result.push(HEX[(byte >> 4) as usize] as char); result.push(HEX[(byte & 15) as usize] as char); }
    }
    result
}

pub fn from_url(path: &str) -> Option<String> {
    let mut raw = Vec::new();
    let mut input = path.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        let byte = if byte == b'%' { nibble(input.next()?)? * 16 + nibble(input.next()?)? } else { byte };
        if byte == 0 { return None; }
        raw.push(byte);
    }
    Some(from_bytes(&raw))
}
