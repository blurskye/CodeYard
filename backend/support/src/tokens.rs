pub const TOKEN_BYTES: usize = 32;
pub const HEX_TOKEN_CHARS: usize = TOKEN_BYTES * 2;
pub const URL_TOKEN_CHARS: usize = (TOKEN_BYTES * 8).div_ceil(6);
pub const PAT_PREFIX: &str = "cy_pat_";
pub const FGPAT_PREFIX: &str = "cy_fgpat_";
pub const ACTIONS_PREFIX: &str = "ghs_";
pub const INSTALLATION_PREFIX: &str = "ghs_";

/// True when `value` is exactly `len` ASCII hex digits, on either compilation target.
pub fn is_hex_token(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
