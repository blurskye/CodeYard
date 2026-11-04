pub const TOKEN_BYTES: usize = 32;
pub const HEX_TOKEN_CHARS: usize = TOKEN_BYTES * 2;
pub const URL_TOKEN_CHARS: usize = (TOKEN_BYTES * 8).div_ceil(6);
pub const PAT_PREFIX: &str = "cy_pat_";
pub const FGPAT_PREFIX: &str = "cy_fgpat_";
pub const ACTIONS_PREFIX: &str = "ghs_";
pub const INSTALLATION_PREFIX: &str = "ghs_";
