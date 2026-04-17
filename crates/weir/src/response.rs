/// Rate limit information extracted from a Discord API response.
#[derive(Debug, Clone)]
pub struct RateLimitHeaders {
    pub bucket: Option<String>,
    pub remaining: Option<u32>,
    pub reset_after: Option<f64>,
    pub is_global: bool,
    pub scope: Option<RateLimitScope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitScope {
    User,
    Global,
    Shared,
}

impl RateLimitHeaders {
    pub fn from_headers(headers: &axum::http::HeaderMap) -> Self {
        Self {
            bucket: header_str(headers, "x-ratelimit-bucket"),
            remaining: header_parse(headers, "x-ratelimit-remaining"),
            reset_after: header_parse(headers, "x-ratelimit-reset-after"),
            is_global: headers
                .get("x-ratelimit-global")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == "true"),
            scope: headers
                .get("x-ratelimit-scope")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| match s {
                    "user" => Some(RateLimitScope::User),
                    "global" => Some(RateLimitScope::Global),
                    "shared" => Some(RateLimitScope::Shared),
                    _ => None,
                }),
        }
    }
}

/// Only used for the bucket hash which must be stored as an owned string.
#[inline]
fn header_str(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

/// Parse a numeric header value without intermediate String allocation.
#[inline]
fn header_parse<T: std::str::FromStr>(headers: &axum::http::HeaderMap, name: &str) -> Option<T> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn extract_rate_limit_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-bucket", "abc123".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "4".parse().unwrap());
        headers.insert("x-ratelimit-reset-after", "1.5".parse().unwrap());
        headers.insert("x-ratelimit-scope", "user".parse().unwrap());

        let rl = RateLimitHeaders::from_headers(&headers);
        assert_eq!(rl.bucket.as_deref(), Some("abc123"));
        assert_eq!(rl.remaining, Some(4));
        assert!((rl.reset_after.unwrap() - 1.5).abs() < f64::EPSILON);
        assert!(!rl.is_global);
        assert_eq!(rl.scope, Some(RateLimitScope::User));
    }

    #[test]
    fn extract_global_rate_limit() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-global", "true".parse().unwrap());

        let rl = RateLimitHeaders::from_headers(&headers);
        assert!(rl.is_global);
    }
}
