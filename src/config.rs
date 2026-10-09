use crate::cookies::{parse_cookies, RawCookie};
use std::env;
use std::path::PathBuf;
use tracing::{info, warn};

/// Amazon cookies embedded at build time. Replace this file (or use
/// ALEXA_COOKIES_JSON / ALEXA_COOKIES_FILE) with a real logged-in session.
const EMBEDDED_ALEXA_COOKIES: &str = include_str!("../config/alexa-cookies.json");

const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";
const DEFAULT_AMAZON_URL: &str = "https://www.amazon.fr";
const DEFAULT_BROWSERLESS_WS_URL: &str =
    "ws://browserless-chrome.browserless-chrome.svc.cluster.local:3000";

/// How the CDP browser is obtained (used only to export Amazon cookies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserMode {
    /// Connect to an existing remote Chrome/browserless endpoint.
    Connect,
    /// Launch a local Chrome with a persistent profile.
    Launch,
}

impl BrowserMode {
    fn from_env() -> Self {
        match non_empty_env("ALEXA_BROWSER_MODE")
            .map(|value| value.to_ascii_lowercase())
            .as_deref()
        {
            Some("launch") | Some("local") => Self::Launch,
            _ => Self::Connect,
        }
    }
}

/// Runtime configuration resolved from the environment.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Address the HTTP/MCP server binds to.
    pub bind_addr: String,
    /// Amazon storefront origin, e.g. "https://www.amazon.fr".
    pub amazon_url: String,
    /// Optional explicit Alexa shopping list id.
    pub alexa_list_id: Option<String>,
    /// Authenticated Amazon session cookies.
    pub alexa_cookies: Vec<RawCookie>,
    /// Whether to connect to a remote browser or launch a local one.
    pub browser_mode: BrowserMode,
    /// WebSocket endpoint of the browserless/Chrome DevTools instance.
    pub browserless_ws_url: String,
    /// Optional browserless security token.
    pub browserless_token: Option<String>,
    /// Optional explicit path to the Chrome binary (launch mode).
    pub chrome_path: Option<String>,
    /// Persistent Chrome profile dir used in launch mode.
    pub chrome_profile_dir: PathBuf,
    /// Run the launched Chrome headless.
    pub chrome_headless: bool,
    /// Where exported cookies are persisted by the cookie tool.
    pub cookies_output_path: PathBuf,
    /// Whether to inject stored cookies before navigating in the browser.
    pub inject_cookies: bool,
    /// Minimum delay between two Amazon requests, in milliseconds.
    pub min_interval_ms: u64,
    /// Extra random jitter added between requests, in milliseconds.
    pub jitter_ms: u64,
    /// Retries on a transient 429/503 before giving up.
    pub max_retries: u32,
    /// Base retry backoff, in milliseconds (doubles each attempt).
    pub backoff_base_ms: u64,
}

impl AppConfig {
    /// Builds the configuration from environment variables.
    pub fn from_env() -> Self {
        let bind_addr = non_empty_env("BIND_ADDR").unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string());
        let amazon_url =
            non_empty_env("AMAZON_URL").unwrap_or_else(|| DEFAULT_AMAZON_URL.to_string());
        let alexa_list_id = non_empty_env("ALEXA_LIST_ID");
        let alexa_cookies = load_alexa_cookies();
        let inject_cookies = bool_env("ALEXA_INJECT_COOKIES", true);
        let browser_mode = BrowserMode::from_env();
        let browserless_ws_url = non_empty_env("BROWSERLESS_WS_URL")
            .unwrap_or_else(|| DEFAULT_BROWSERLESS_WS_URL.to_string());
        let browserless_token = non_empty_env("BROWSERLESS_TOKEN");
        let chrome_path = non_empty_env("ALEXA_CHROME_PATH");
        let data_dir = config_dir();
        let chrome_profile_dir = non_empty_env("ALEXA_CHROME_PROFILE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("chrome"));
        let chrome_headless = bool_env("ALEXA_HEADLESS", false);
        let cookies_output_path = non_empty_env("ALEXA_COOKIES_OUTPUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("alexa-cookies.json"));

        Self {
            bind_addr,
            amazon_url,
            alexa_list_id,
            alexa_cookies,
            inject_cookies,
            browser_mode,
            browserless_ws_url,
            browserless_token,
            chrome_path,
            chrome_profile_dir,
            chrome_headless,
            cookies_output_path,
            min_interval_ms: int_env("ALEXA_MIN_INTERVAL_MS", 1000),
            jitter_ms: int_env("ALEXA_JITTER_MS", 400),
            max_retries: int_env("ALEXA_MAX_RETRIES", 3) as u32,
            backoff_base_ms: int_env("ALEXA_BACKOFF_BASE_MS", 1500),
        }
    }

    /// Amazon origin without a trailing slash.
    pub fn amazon_origin(&self) -> String {
        self.amazon_url.trim_end_matches('/').to_string()
    }

    /// Amazon host, e.g. "www.amazon.fr".
    pub fn amazon_host(&self) -> String {
        let without_scheme = self
            .amazon_url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.amazon_url);
        without_scheme
            .split('/')
            .next()
            .unwrap_or(without_scheme)
            .split(':')
            .next()
            .unwrap_or("")
            .to_string()
    }
}

/// Base directory for persisted state (~/.alexa-mcp by default, fallback to /tmp/.alexa-mcp).
fn config_dir() -> PathBuf {
    if let Some(dir) = non_empty_env("ALEXA_CONFIG_DIR") {
        let path = PathBuf::from(dir);
        if std::fs::create_dir_all(&path).is_ok() {
            return path;
        }
    }
    if let Some(home) = non_empty_env("HOME").or_else(|| non_empty_env("USERPROFILE")) {
        let path = PathBuf::from(home).join(".alexa-mcp");
        if std::fs::create_dir_all(&path).is_ok() {
            return path;
        }
    }
    let fallback = PathBuf::from("/tmp/.alexa-mcp");
    if std::fs::create_dir_all(&fallback).is_ok() {
        return fallback;
    }
    PathBuf::from(".alexa-mcp")
}

/// Resolves the Amazon cookie set, in order of precedence:
///
/// 1. ALEXA_COOKIES_JSON - a raw JSON array passed inline.
/// 2. ALEXA_COOKIES_FILE - the path to a JSON file.
/// 3. the cookies embedded at build time in config/alexa-cookies.json.
fn load_alexa_cookies() -> Vec<RawCookie> {
    let (source, origin) = if let Some(json) = non_empty_env("ALEXA_COOKIES_JSON") {
        (json, "ALEXA_COOKIES_JSON")
    } else if let Some(json) = non_empty_env("AMAZON_COOKIES_JSON") {
        (json, "AMAZON_COOKIES_JSON")
    } else if let Some(path) = non_empty_env("ALEXA_COOKIES_FILE") {
        match std::fs::read_to_string(&path) {
            Ok(contents) => (contents, "ALEXA_COOKIES_FILE"),
            Err(error) => {
                warn!(
                    "Could not read ALEXA_COOKIES_FILE '{path}': {error}; using embedded cookies"
                );
                (EMBEDDED_ALEXA_COOKIES.to_string(), "embedded config")
            }
        }
    } else {
        (EMBEDDED_ALEXA_COOKIES.to_string(), "embedded config")
    };

    match parse_cookies(&source) {
        Ok(cookies) if !cookies.is_empty() => {
            info!("Loaded {} Amazon cookies from {origin}", cookies.len());
            cookies
        }
        Ok(_) => {
            warn!(
                "No Amazon cookies found in {origin}; set ALEXA_COOKIES_JSON or ALEXA_COOKIES_FILE"
            );
            Vec::new()
        }
        Err(error) => {
            warn!("Invalid Amazon cookie JSON in {origin}: {error}; continuing without cookies");
            Vec::new()
        }
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn int_env(key: &str, fallback: u64) -> u64 {
    non_empty_env(key)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(fallback)
}

fn bool_env(key: &str, fallback: bool) -> bool {
    match non_empty_env(key).map(|value| value.to_ascii_lowercase()) {
        Some(value) => matches!(value.as_str(), "1" | "true" | "yes" | "on"),
        None => fallback,
    }
}
