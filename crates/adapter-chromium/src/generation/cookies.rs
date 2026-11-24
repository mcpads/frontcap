use super::*;
use cdp::protocol::network;
use crate::encode::canonical_origin_host;
use frontcap_core::intervention::{CookiePartition, CookiePriority, CookieSameSite, CookieSource, CookieSourceScheme, CookieTiming, InitialCookie};

fn invalid(message: &str) -> ChromiumError { ChromiumError::InvalidArgument(message.into()) }

pub fn validate_initial_cookies(cookies: &[InitialCookie], initial_date_ms: u64) -> Result<(), ChromiumError> {
    if cookies.len() > 64 || serde_json::to_vec(cookies).unwrap().len() > 262144 {
        return Err(invalid("initial cookies exceed 64 entries or 256 KiB"));
    }
    let mut identities = std::collections::BTreeSet::new();
    for c in cookies {
        if let Some(timing) = &c.timing {
            let epoch = windows_epoch_us(initial_date_ms)?;
            if c.expires_at_ms.is_some() || timing.creation_age_us >= epoch
                || timing.last_access_age_us > timing.creation_age_us
                || timing.last_update_age_us > timing.creation_age_us
                || timing.remaining_lifetime_us.is_some_and(|us| us == 0 || us > 400 * 24 * 60 * 60 * 1_000_000) {
                return Err(invalid("cookie timing must preserve nonnegative ages, positive remaining lifetime within 400 days, and cannot accompany expires_at_ms"));
            }
        }
        if let Some(expiry) = c.expires_at_ms {
            // Refuse native lifetime clamping: readback must preserve the requested instant.
            if expiry > 9_007_199_254_740_991 || expiry <= initial_date_ms
                || expiry - initial_date_ms > 400 * 24 * 60 * 60 * 1000 {
                return Err(invalid("cookie expires_at_ms must be an exact JSON integer after initial_date_ms and within 400 days"));
            }
        }
        let host = canonical_origin_host(&c.origin)?;
        if let Some(source) = c.source {
            if source.port != -1 && !(1..=65535).contains(&source.port) {
                return Err(invalid("cookie source port must be -1 or between 1 and 65535"));
            }
            if c.secure && source.scheme == CookieSourceScheme::NonSecure {
                return Err(invalid("Secure cookies cannot have a non-secure source scheme"));
            }
        }
        if c.name.is_empty() || !c.name.bytes().all(|b| b > 32 && b < 127 && !b"()<>@,;:\\\"/[]?={}".contains(&b))
            || !c.value.bytes().all(|b| b == 33 || (35..=43).contains(&b) || (45..=58).contains(&b) || (60..=91).contains(&b) || (93..=126).contains(&b))
            || c.name.len() + c.value.len() > 4096 {
            return Err(invalid("cookie name/value must use cookie ASCII syntax and fit 4096 bytes"));
        }
        if !c.path.starts_with('/') || c.path.len() > 1024 || !c.path.bytes().all(|b| (32..127).contains(&b) && b != b';') {
            return Err(invalid("cookie path requires an absolute ASCII path of at most 1024 bytes"));
        }
        if let Some(domain) = &c.domain {
            let suffix = domain.strip_prefix('.').ok_or_else(|| invalid("domain cookies require a leading dot; omit domain for host-only cookies"))?;
            canonical_origin_host(&format!("https://{suffix}"))?;
            if suffix.contains(':') || suffix.parse::<std::net::IpAddr>().is_ok() || (host != suffix && !host.ends_with(domain)) {
                return Err(invalid("cookie domain must contain its origin host and cannot be an IP address"));
            }
        }
        if (c.secure && !c.origin.starts_with("https://")) || ((c.same_site == CookieSameSite::None || c.partition.is_some()) && !c.secure)
            || (c.name.starts_with("__Secure-") && !c.secure)
            || (c.name.starts_with("__Host-") && (!c.secure || c.domain.is_some() || c.path != "/"))
            || ((c.name.starts_with("__Http-") || c.name.starts_with("__Host-Http-")) && (!c.secure || !c.http_only)) {
            return Err(invalid("cookie security attributes conflict with its origin, prefix, SameSite or partition"));
        }
        if let Some(partition) = &c.partition {
            let site_host = canonical_origin_host(&partition.top_level_site)?;
            let scheme = if partition.top_level_site.starts_with("https:") { "https" } else { "http" };
            if partition.top_level_site != format!("{scheme}://{site_host}") {
                return Err(invalid("cookie partition site cannot include a port"));
            }
        }
        let identity = (c.domain.as_deref().unwrap_or(host).to_string(), c.path.clone(), c.name.clone(),
            c.partition.as_ref().map(|p| (p.top_level_site.clone(), p.has_cross_site_ancestor)));
        if !identities.insert(identity) { return Err(invalid("initial cookies contain duplicate storage identities")); }
    }
    Ok(())
}

pub(super) fn same_site(value: CookieSameSite) -> Option<network::CookieSameSite> {
    match value { CookieSameSite::Strict => Some(network::CookieSameSite::Strict), CookieSameSite::Lax => Some(network::CookieSameSite::Lax), CookieSameSite::None => Some(network::CookieSameSite::None), CookieSameSite::Unspecified => None }
}

fn priority(value: CookiePriority) -> network::CookiePriority {
    match value { CookiePriority::Low => network::CookiePriority::Low, CookiePriority::Medium => network::CookiePriority::Medium, CookiePriority::High => network::CookiePriority::High }
}

/// Preserve the native storage identity instead of assigning the top page's origin.
fn captured_cookie(c: network::Cookie, now_us: u64) -> Result<InitialCookie, ChromiumError> {
    if c.partition_key_opaque.is_some() {
        return Err(invalid("live cookie capture cannot reconstruct an opaque or nonce partition"));
    }
    let (scheme, default_port) = match c.source_scheme {
        network::CookieSourceScheme::Secure => ("https", 443),
        network::CookieSourceScheme::NonSecure => ("http", 80),
        network::CookieSourceScheme::Unset => if c.secure { ("https", 443) } else { ("http", 80) },
    };
    if c.source_port != -1 && !(1..=65535).contains(&c.source_port) {
        return Err(invalid("live cookie capture received an invalid source port"));
    }
    let host = c.domain.strip_prefix('.').unwrap_or(&c.domain);
    let origin = if c.source_port == -1 || c.source_port == default_port { format!("{scheme}://{host}") }
        else { format!("{scheme}://{host}:{}", c.source_port) };
    let timestamps = c.frontcap_timestamps.as_ref().ok_or_else(|| invalid("native cookie timestamps are missing"))?;
    let age = |s: &str| -> Result<u64, ChromiumError> {
        let time = s.parse::<u64>().map_err(|_| invalid("invalid native cookie timestamp"))?;
        if time == 0 { return Err(invalid("native cookie timestamp is null")); }
        now_us.checked_sub(time).ok_or_else(|| invalid("native cookie timestamp is later than the capture boundary"))
    };
    let expiry = timestamps.expiry.parse::<u64>().map_err(|_| invalid("invalid native cookie expiry"))?;
    let remaining_lifetime_us = if expiry == 0 { None } else {
        Some(expiry.checked_sub(now_us).filter(|v| *v > 0).ok_or_else(|| invalid("native cookie is already expired"))?)
    };
    if c.session != remaining_lifetime_us.is_none() { return Err(invalid("native cookie lifetime disagrees with its timestamps")); }
    let timing = Some(CookieTiming { creation_age_us: age(&timestamps.creation)?, last_access_age_us: age(&timestamps.last_access)?, last_update_age_us: age(&timestamps.last_update)?, remaining_lifetime_us });
    let source = (c.source_scheme == network::CookieSourceScheme::Unset || c.source_port == -1).then_some(CookieSource {
        scheme: match c.source_scheme { network::CookieSourceScheme::Unset => CookieSourceScheme::Unset, network::CookieSourceScheme::NonSecure => CookieSourceScheme::NonSecure, network::CookieSourceScheme::Secure => CookieSourceScheme::Secure },
        port: c.source_port,
    });
    Ok(InitialCookie {
        source, timing, origin, name: c.name, value: c.value, expires_at_ms: None,
        domain: c.domain.starts_with('.').then_some(c.domain), path: c.path,
        secure: c.secure, http_only: c.http_only,
        same_site: match c.same_site {
            Some(network::CookieSameSite::Strict) => CookieSameSite::Strict,
            Some(network::CookieSameSite::Lax) => CookieSameSite::Lax,
            Some(network::CookieSameSite::None) => CookieSameSite::None,
            None => CookieSameSite::Unspecified,
        },
        priority: match c.priority { network::CookiePriority::Low => CookiePriority::Low, network::CookiePriority::Medium => CookiePriority::Medium, network::CookiePriority::High => CookiePriority::High },
        partition: c.partition_key.map(|p| CookiePartition { top_level_site: p.top_level_site, has_cross_site_ancestor: p.has_cross_site_ancestor }),
    })
}

fn cookie_source(c: &InitialCookie) -> Result<(network::CookieSourceScheme, i64), ChromiumError> {
    if let Some(source) = c.source {
        return Ok((match source.scheme { CookieSourceScheme::Unset => network::CookieSourceScheme::Unset, CookieSourceScheme::NonSecure => network::CookieSourceScheme::NonSecure, CookieSourceScheme::Secure => network::CookieSourceScheme::Secure }, source.port));
    }
    let host = canonical_origin_host(&c.origin)?;
    let secure_origin = c.origin.starts_with("https://");
    let authority = c.origin.split_once("://").expect("validated origin").1;
    let port = match authority[host.len()..].strip_prefix(':') {
        Some(port) => port.parse::<i64>().map_err(|_| invalid("invalid cookie source port"))?,
        None => if secure_origin { 443 } else { 80 },
    };
    Ok((if secure_origin { network::CookieSourceScheme::Secure } else { network::CookieSourceScheme::NonSecure }, port))
}

fn windows_epoch_us(initial_date_ms: u64) -> Result<u64, ChromiumError> {
    initial_date_ms.checked_mul(1000).and_then(|us| us.checked_add(11_644_473_600_000_000))
        .filter(|us| *us < i64::MAX as u64 - 400 * 24 * 60 * 60 * 1_000_000)
        .ok_or_else(|| invalid("cookie clock is outside the native timestamp range"))
}

fn native_timestamps(timing: &CookieTiming, initial_date_ms: u64) -> Result<network::FrontcapCookieTimestamps, ChromiumError> {
    let now = windows_epoch_us(initial_date_ms)?;
    let stamp = |age| now.checked_sub(age).map(|us| us.to_string()).ok_or_else(|| invalid("cookie age predates native time"));
    Ok(network::FrontcapCookieTimestamps {
        creation: stamp(timing.creation_age_us)?, last_access: stamp(timing.last_access_age_us)?,
        last_update: stamp(timing.last_update_age_us)?,
        expiry: timing.remaining_lifetime_us.map_or_else(|| "0".into(), |us| (now + us).to_string()),
    })
}

pub(super) fn cookie_param(c: &InitialCookie, initial_date_ms: u64) -> Result<network::CookieParam, ChromiumError> {
    let (source_scheme, source_port) = cookie_source(c)?;
    Ok(network::CookieParam {
        frontcap_timestamps: c.timing.as_ref().map(|t| native_timestamps(t, initial_date_ms)).transpose()?,
        // Chromium's MakeCookieFromProtocolValues ORs an HTTPS url into the Secure attribute.
        // Supplying an explicit domain and source metadata preserves a non-Secure cookie set
        // from an HTTPS origin. A domain without a leading dot remains host-only in Chromium.
        url: None, domain: Some(c.domain.clone().unwrap_or(canonical_origin_host(&c.origin)?.into())), path: Some(c.path.clone()),
        secure: Some(c.secure), http_only: Some(c.http_only), same_site: same_site(c.same_site),
        partition_key: c.partition.as_ref().map(|p| network::CookiePartitionKey { top_level_site: p.top_level_site.clone(), has_cross_site_ancestor: p.has_cross_site_ancestor }),
        name: c.name.clone(), value: c.value.clone(), expires: c.expires_at_ms.map(|ms| network::TimeSinceEpoch(ms as f64 / 1000.0)), priority: Some(priority(c.priority)),
        source_scheme: Some(source_scheme), source_port: Some(source_port),
    })
}

impl Generation {
    pub(super) async fn live_cookies(&mut self) -> Result<Vec<InitialCookie>, ChromiumError> {
        let found = self.page.call(&cdp::protocol::storage::GetCookiesParams::default()).await?.cookies;
        let now_us = windows_epoch_us(self.profile.initial_date_ms)?.checked_add(self.now.0).ok_or_else(|| invalid("cookie capture clock overflow"))?;
        let mut cookies = found.into_iter().map(|c| captured_cookie(c, now_us)).collect::<Result<Vec<_>, _>>()?;
        cookies.sort_by_cached_key(|c| (c.origin.clone(), c.domain.clone(), c.path.clone(), c.name.clone(),
            c.partition.as_ref().map(|p| (p.top_level_site.clone(), p.has_cross_site_ancestor))));
        validate_initial_cookies(&cookies, self.profile.initial_date_ms)?;
        Ok(cookies)
    }

    pub(super) async fn initialize_cookies(&mut self) -> Result<(), ChromiumError> {
        use cdp::protocol::storage;
        let cookies = self.profile.initial_cookies.clone();
        if cookies.is_empty() { return Ok(()) }
        let params = cookies.iter().map(|c| cookie_param(c, self.profile.initial_date_ms)).collect::<Result<Vec<_>, _>>()?;
        self.page.call(&storage::SetCookiesParams::new(params)).await?;
        let actual = self.page.call(&storage::GetCookiesParams::default()).await?.cookies;
        for c in &cookies {
            let expected_timestamps = c.timing.as_ref().map(|t| native_timestamps(t, self.profile.initial_date_ms)).transpose()?;
            let (source_scheme, source_port) = cookie_source(c)?;
            let domain = c.domain.as_deref().unwrap_or(canonical_origin_host(&c.origin)?);
            let matching: Vec<_> = actual.iter().filter(|a| a.name == c.name && a.domain == domain && a.path == c.path
                && match (&a.partition_key, &c.partition) {
                    (None, None) => true,
                    (Some(a), Some(p)) => a.top_level_site == p.top_level_site && a.has_cross_site_ancestor == p.has_cross_site_ancestor,
                    _ => false,
                }).collect();
            if matching.len() != 1 || !matching.iter().all(|a| a.value == c.value && a.secure == c.secure && a.http_only == c.http_only
                && a.source_scheme == source_scheme && a.source_port == source_port
                && if let Some(timing) = &c.timing {
                    a.session == timing.remaining_lifetime_us.is_none()
                        && a.frontcap_timestamps == expected_timestamps
                } else { match c.expires_at_ms {
                    None => a.session,
                    Some(ms) => !a.session && a.expires == ms as f64 / 1000.0,
                } }
                && a.same_site == same_site(c.same_site) && a.priority == priority(c.priority) && a.partition_key_opaque.is_none()) {
                return Err(ChromiumError::InterventionUncertain("initial cookie readback did not match; generation is not ready".into()));
            }
        }
        self.record(self.now, EventKind::CookiesInitialized(cookies));
        self.commit()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "cookies_tests.rs"]
mod tests;
