pub use frontcap_core::encode::{base64, unbase64};

/// `scheme://host[:port]` of an absolute URL with an authority, or `None`.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if scheme.is_empty() || authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// Accept only explicit canonical origins; do not let URL parsing silently repair storage scopes.
pub(crate) fn canonical_origin_host(origin: &str) -> Result<&str, crate::ChromiumError> {
    let invalid = |message: &str| crate::ChromiumError::InvalidArgument(message.into());
    let authority = origin.strip_prefix("https://").or_else(|| origin.strip_prefix("http://"))
        .ok_or_else(|| invalid("origin requires canonical HTTP(S) scheme and host"))?;
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']').ok_or_else(|| invalid("invalid IPv6 host"))?;
        let host = &authority[..=end];
        let address = host[1..host.len()-1].parse::<std::net::Ipv6Addr>().map_err(|_| invalid("invalid IPv6 host"))?;
        if format!("[{address}]") != host { return Err(invalid("IPv6 host must be canonical")); }
        (host, &authority[end+1..])
    } else {
        let end = authority.find(':').unwrap_or(authority.len());
        let host = &authority[..end];
        if host.is_empty() || host.len() > 253 || host.split('.').any(|label| label.is_empty() || label.len() > 63
            || label.starts_with('-') || label.ends_with('-') || !label.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')) {
            return Err(invalid("origin host requires canonical ASCII DNS labels or an IP address"));
        }
        // Numeric hosts must not use URL-parser abbreviations or noncanonical bases.
        if host.rsplit('.').next().is_some_and(|last| last.bytes().all(|c| c.is_ascii_digit()))
            && host.parse::<std::net::Ipv4Addr>().ok().is_none_or(|ip| ip.to_string() != host) {
            return Err(invalid("IPv4 host must be canonical"));
        }
        (host, &authority[end..])
    };
    if !port.is_empty() {
        let port = port.strip_prefix(':').ok_or_else(|| invalid("origin cannot contain a path or credentials"))?;
        let number = port.parse::<u16>().map_err(|_| invalid("invalid origin port"))?;
        if number == 0 || number.to_string() != port || (number == 443 && origin.starts_with("https:")) || (number == 80 && origin.starts_with("http:")) {
            return Err(invalid("origin port must be canonical and nondefault"));
        }
    }
    Ok(host)
}

#[cfg(test)]
#[path = "encode_tests.rs"]
mod tests;
