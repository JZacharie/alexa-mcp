use crate::config::{AppConfig, BrowserMode};
use crate::cookies::{is_amazon_domain, RawCookie};
use anyhow::{anyhow, Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::Handler;
use chromiumoxide::Page;
use futures::StreamExt;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// How long to wait after a top-level navigation before reading the session.
const CHALLENGE_SETTLE: Duration = Duration::from_millis(2000);

/// How many times a browser connect + navigation is attempted before failing.
const CONNECT_ATTEMPTS: usize = 3;

/// Delay between two browser session attempts.
const CONNECT_RETRY_BACKOFF: Duration = Duration::from_millis(1500);

/// A browser page owned for the duration of one tool call.
///
/// The page is closed when the session is dropped, including on an early error
/// return, so a long-lived browser cannot accumulate tabs.
pub struct Session {
    page: Page,
}

impl Session {
    fn new(page: Page) -> Self {
        Self { page }
    }
}

impl Deref for Session {
    type Target = Page;

    fn deref(&self) -> &Page {
        &self.page
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let page = self.page.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = page.close().await;
            });
        }
    }
}

/// Browser helper used only to obtain the Amazon cookies.
#[derive(Clone)]
pub struct BrowserClient {
    mode: BrowserMode,
    ws_url: String,
    token: Option<String>,
    chrome_path: Option<String>,
    chrome_profile_dir: PathBuf,
    headless: bool,
    slot: Arc<Mutex<Option<Browser>>>,
}

impl BrowserClient {
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            mode: config.browser_mode,
            ws_url: config.browserless_ws_url.clone(),
            token: config.browserless_token.clone(),
            chrome_path: config.chrome_path.clone(),
            chrome_profile_dir: config.chrome_profile_dir.clone(),
            headless: config.chrome_headless,
            slot: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns a session page navigated to origin, without pre-injected cookies.
    pub async fn ensure_page(&self, origin: &str) -> Result<Session> {
        self.ensure_page_with_cookies(origin, &[]).await
    }

    /// Returns a session page navigated to origin, injecting the given cookies
    /// *before* navigation so the session is active immediately.
    pub async fn ensure_page_with_cookies(
        &self,
        origin: &str,
        cookies: &[RawCookie],
    ) -> Result<Session> {
        let mut last_error = None;
        for attempt in 1..=CONNECT_ATTEMPTS {
            match self.create_session(origin, cookies).await {
                Ok(session) => return Ok(session),
                Err(error) => {
                    warn!("Browser session attempt {attempt}/{CONNECT_ATTEMPTS} failed: {error:?}");
                    self.reset().await;
                    last_error = Some(error);
                    if attempt < CONNECT_ATTEMPTS {
                        tokio::time::sleep(CONNECT_RETRY_BACKOFF).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("The browser session could not be established")))
    }

    async fn create_session(&self, origin: &str, cookies: &[RawCookie]) -> Result<Session> {
        let page = self.create_cached_page().await?;
        let session = Session::new(page);

        if !cookies.is_empty() {
            let injected = self
                .inject_cookies(&session, cookies, origin)
                .await
                .context("Failed to inject the stored Amazon cookies")?;
            info!("Injected {injected} stored Amazon cookie(s) before navigating to {origin}");
        }

        self.open_origin(&session, origin).await?;
        Ok(session)
    }

    /// Injects cookies into the page's CDP session.
    pub async fn inject_cookies(
        &self,
        page: &Page,
        cookies: &[RawCookie],
        default_url: &str,
    ) -> Result<usize> {
        let mut params = Vec::with_capacity(cookies.len());
        for cookie in cookies {
            match cookie.to_cookie_param(default_url) {
                Ok(param) => params.push(param),
                Err(error) => warn!("Skipping invalid cookie '{}': {error}", cookie.name),
            }
        }

        if params.is_empty() {
            warn!("No valid cookies to inject");
            return Ok(0);
        }

        let count = params.len();
        page.set_cookies(params)
            .await
            .context("Failed to set cookies through CDP Network.setCookies")?;
        debug!("Injected {count} cookies into the CDP session");
        Ok(count)
    }

    /// Navigates the page to the Amazon origin applying anti-bot stealth scripts.
    pub async fn open_origin(&self, page: &Page, origin: &str) -> Result<()> {
        debug!("Navigating to origin {origin}");
        const STEALTH_SCRIPT: &str = r#"(() => {
            try {
                Object.defineProperty(navigator, 'webdriver', {
                    get: () => undefined,
                    configurable: true,
                });
            } catch (_) {}
            try {
                window.chrome = window.chrome || { runtime: {} };
            } catch (_) {}
        })()"#;

        if let Ok(params) = EvaluateParams::builder()
            .expression(STEALTH_SCRIPT)
            .return_by_value(true)
            .build()
        {
            let _ = page.evaluate_expression(params).await;
        }

        page.goto(origin)
            .await
            .with_context(|| format!("Failed to navigate to {origin}"))?;

        if let Ok(params) = EvaluateParams::builder()
            .expression(STEALTH_SCRIPT)
            .return_by_value(true)
            .build()
        {
            let _ = page.evaluate_expression(params).await;
        }

        tokio::time::sleep(CHALLENGE_SETTLE).await;
        Ok(())
    }

    /// Reads the live Amazon cookies from the page.
    pub async fn get_cookies(&self, page: &Page) -> Result<Vec<RawCookie>> {
        let cookies = page
            .get_cookies()
            .await
            .context("Failed to read cookies through CDP Network.getCookies")?;
        Ok(cookies
            .iter()
            .filter(|cookie| is_amazon_domain(&cookie.domain))
            .map(RawCookie::from_cdp)
            .collect())
    }

    async fn create_cached_page(&self) -> Result<Page> {
        let mut slot = self.slot.lock().await;
        if slot.is_none() {
            *slot = Some(self.launch_or_connect().await?);
        }
        let browser = slot
            .as_ref()
            .context("Browser connection is not available")?;
        browser
            .new_page("about:blank")
            .await
            .context("Failed to open a new browser page")
    }

    async fn reset(&self) {
        let mut slot = self.slot.lock().await;
        *slot = None;
    }

    async fn launch_or_connect(&self) -> Result<Browser> {
        match self.mode {
            BrowserMode::Connect => {
                let endpoint = self.endpoint();
                info!("Connecting to remote CDP endpoint: {endpoint}");
                let (browser, handler) = Browser::connect(&endpoint)
                    .await
                    .context("Failed to connect to the browserless Chrome CDP WebSocket")?;
                spawn_handler(handler);
                Ok(browser)
            }
            BrowserMode::Launch => {
                let mut builder = BrowserConfig::builder()
                    .disable_default_args()
                    .arg("--no-first-run")
                    .arg("--no-default-browser-check")
                    .arg("about:blank")
                    .user_data_dir(&self.chrome_profile_dir);
                if let Some(path) = self.chrome_path.as_deref().filter(|p| !p.trim().is_empty()) {
                    builder = builder.chrome_executable(path);
                }
                builder = if self.headless {
                    builder.new_headless_mode()
                } else {
                    builder.with_head()
                };
                let config = builder.build().map_err(|error| anyhow!(error))?;

                info!(
                    "Launching local Chrome (profile {})",
                    self.chrome_profile_dir.display()
                );
                let (browser, handler) = Browser::launch(config)
                    .await
                    .context("Could not launch local Chrome; set ALEXA_CHROME_PATH")?;
                spawn_handler(handler);
                Ok(browser)
            }
        }
    }

    fn endpoint(&self) -> String {
        match &self.token {
            Some(token) if !token.is_empty() => {
                if self.ws_url.contains('?') {
                    format!("{}&token={token}", self.ws_url)
                } else {
                    format!("{}?token={token}", self.ws_url)
                }
            }
            _ => self.ws_url.clone(),
        }
    }
}

fn spawn_handler(mut handler: Handler) {
    tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(error) = event {
                error!("CDP handler stream error: {error:?}");
                break;
            }
        }
    });
}
