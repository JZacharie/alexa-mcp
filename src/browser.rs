use crate::config::{AppConfig, BrowserMode};
use crate::cookies::{is_amazon_domain, RawCookie};
use anyhow::{anyhow, Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::Handler;
use chromiumoxide::Page;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

/// How long to wait after a top-level navigation before reading the session.
const CHALLENGE_SETTLE: Duration = Duration::from_millis(2000);

/// Browser helper used only to obtain the Amazon cookies.
///
/// The shopping list itself is read over plain HTTPS with a cookie replay
/// (see crate::alexa), so the browser is required only when exporting a fresh
/// Amazon session.
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

    /// Returns a page navigated to origin, creating the browser connection on
    /// first use and reconnecting once if it became unusable.
    pub async fn ensure_page(&self, origin: &str) -> Result<Page> {
        match self.create_cached_page().await {
            Ok(page) => {
                self.open_origin(&page, origin).await?;
                Ok(page)
            }
            Err(error) => {
                warn!("Browser connection unusable ({error:?}); reconnecting");
                self.reset().await;
                let page = self.create_cached_page().await?;
                self.open_origin(&page, origin).await?;
                Ok(page)
            }
        }
    }

    /// Navigates the page to the Amazon origin so it holds the session cookies.
    pub async fn open_origin(&self, page: &Page, origin: &str) -> Result<()> {
        debug!("Navigating to origin {origin}");
        page.goto(origin)
            .await
            .with_context(|| format!("Failed to navigate to {origin}"))?;
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
