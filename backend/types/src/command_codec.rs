//! Target-independent workflow-command escaping, shared with browser log rendering.

pub fn escape_data(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

pub fn escape(value: &str) -> String {
    escape_data(value).replace(':', "%3A").replace(',', "%2C")
}

pub fn unescape_data(value: &str) -> String {
    value
        .replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%25", "%")
}

pub fn unescape_property(value: &str) -> String {
    unescape_data(&value.replace("%3A", ":").replace("%2C", ","))
}
