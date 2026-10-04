//! Amazon/Alexa cookie handling.
//!
//! Cookies are provided as a JSON array in the format exported by browser
//! extensions such as Cookie-Editor or EditThisCookie. The same shape is used
//! when exporting the live session read from the browser via CDP, so an export
//! can be fed straight back into ALEXA_COOKIES_JSON or the embedded file.

use chromiumoxide::cdp::browser_protocol::network::Cookie;
use serde::{Deserialize, Serialize};

/// A single cookie in the browser-extension export format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawCookie {
    pub name: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, rename = "httpOnly", skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    #[serde(
        default,
        rename = "expirationDate",
        skip_serializing_if = "Option::is_none"
    )]
    pub expiration_date: Option<f64>,
    #[serde(default, rename = "sameSite", skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
    #[serde(default, rename = "hostOnly", skip_serializing_if = "Option::is_none")]
    pub host_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_id: Option<String>,
}

impl RawCookie {
    /// Converts a cookie read from the browser (CDP Network.Cookie) into the
    /// export format used by config/alexa-cookies.json.
    pub fn from_cdp(cookie: &Cookie) -> Self {
        let same_site = cookie
            .same_site
            .as_ref()
            .map(|value| value.as_ref().to_string());
        Self {
            name: cookie.name.clone(),
            value: cookie.value.clone(),
            domain: Some(cookie.domain.clone()),
            path: Some(cookie.path.clone()),
            http_only: Some(cookie.http_only),
            secure: Some(cookie.secure),
            expiration_date: if cookie.session {
                None
            } else {
                Some(cookie.expires)
            },
            same_site,
            host_only: Some(!cookie.domain.starts_with('.')),
            session: Some(cookie.session),
            store_id: None,
        }
    }
}

/// True when the cookie belongs to an Amazon storefront (amazon.fr,
/// www.amazon.com, alexa.amazon.com, ...).
pub fn is_amazon_domain(domain: &str) -> bool {
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    domain.split('.').any(|label| label == "amazon")
}

/// Parses a JSON array of exported cookies.
pub fn parse_cookies(json: &str) -> Result<Vec<RawCookie>, serde_json::Error> {
    serde_json::from_str(json)
}

/// Serializes cookies back to the export format.
pub fn serialize_cookies(cookies: &[RawCookie]) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(cookies).map(|json| json + "\n")
}

/// Builds the value of the HTTP Cookie header for the given host.
pub fn cookie_header(cookies: &[RawCookie], host: &str) -> Option<String> {
    let host = host.trim_start_matches('.').to_ascii_lowercase();
    let pairs: Vec<String> = cookies
        .iter()
        .filter(|cookie| match cookie.domain.as_deref() {
            Some(domain) if !domain.trim().is_empty() => {
                let domain = domain.trim_start_matches('.').to_ascii_lowercase();
                host == domain || host.ends_with(&format!(".{domain}"))
            }
            // Host-only cookies apply to the exact host.
            _ => true,
        })
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect();

    if pairs.is_empty() {
        None
    } else {
        Some(pairs.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, value: &str) -> RawCookie {
        RawCookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: Some(".amazon.fr".to_string()),
            path: Some("/".to_string()),
            http_only: Some(true),
            secure: Some(true),
            expiration_date: Some(1_795_759_131.0),
            same_site: Some("lax".to_string()),
            host_only: Some(false),
            session: None,
            store_id: None,
        }
    }

    #[test]
    fn parses_exported_cookie_array() {
        let json = r#"[{"name":"session-id","value":"123","domain":".amazon.fr",
            "path":"/","httpOnly":true,"secure":true,"session":false,
            "expirationDate":1795759131.2,"sameSite":"lax","hostOnly":false,"storeId":null}]"#;
        let parsed = parse_cookies(json).expect("cookie array should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "session-id");
        assert_eq!(parsed[0].expiration_date, Some(1_795_759_131.2));
    }

    #[test]
    fn amazon_domain_matching() {
        assert!(is_amazon_domain(".amazon.fr"));
        assert!(is_amazon_domain("www.amazon.fr"));
        assert!(is_amazon_domain("www.amazon.com"));
        assert!(is_amazon_domain("alexa.amazon.com"));
        assert!(!is_amazon_domain("example.com"));
        assert!(!is_amazon_domain("notamazon.fr"));
    }

    #[test]
    fn cookie_header_filters_by_host() {
        let cookies = vec![
            cookie("session-id", "123"),
            RawCookie {
                domain: Some(".example.com".to_string()),
                ..cookie("other", "x")
            },
        ];
        let header = cookie_header(&cookies, "www.amazon.fr").expect("header");
        assert_eq!(header, "session-id=123");
    }

    #[test]
    fn round_trips_through_serialization() {
        let cookies = vec![cookie("session-id", "123")];
        let json = serialize_cookies(&cookies).expect("serialize");
        let parsed = parse_cookies(&json).expect("parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "session-id");
    }
}
