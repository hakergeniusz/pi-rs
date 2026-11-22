//! `trustedHttpUrl` (`packages/ai/src/auth/oauth/trusted-url.ts:1-10`).
//!
//! Login flows open server-supplied URLs with the platform handler, so only
//! `http:`/`https:` values may reach it. Upstream normalizes with the WHATWG
//! `URL` parser (`url.href`); the port uses a conservative string check and
//! keeps the trimmed value, documented as a deviation in `WIRING.md`.

/// The normalized `http:`/`https:` URL, or `None` (upstream `null`).
#[must_use]
pub fn trusted_http_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.chars().any(char::is_control) {
        return None;
    }
    let (scheme, rest) = trimmed.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_http_and_https() {
        assert_eq!(
            trusted_http_url(" https://example.com/code?a=b "),
            Some("https://example.com/code?a=b".to_string())
        );
        assert_eq!(trusted_http_url("file:///etc/passwd"), None);
        assert_eq!(trusted_http_url("javascript:alert(1)"), None);
        assert_eq!(trusted_http_url("https://"), None);
        assert_eq!(trusted_http_url("https://exa mple.com"), None);
    }
}
