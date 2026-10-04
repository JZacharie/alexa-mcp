use crate::browser::BrowserClient;
use crate::config::AppConfig;
use crate::cookies::{cookie_header, serialize_cookies, RawCookie};
use crate::throttle::Throttler;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Mobile Alexa-app user agent. The reverse-engineered endpoints are the ones
/// used by the iPhone Alexa app, and Amazon expects this fingerprint.
const MOBILE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 13_5_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148 PitanguiBridge/2.2.345247.0-[HARDWARE=iPhone10_4][SOFTWARE=13.5.1]";

/// HTTP statuses worth retrying (rate limiting / temporary unavailability).
const RETRYABLE_STATUSES: [u16; 2] = [429, 503];

/// A single Alexa shopping list item. Unknown fields are preserved so the item
/// can be sent back verbatim on update/delete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShoppingItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub completed: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The Alexa shopping list (and a few derived counters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShoppingList {
    pub list_id: Option<String>,
    pub items: Vec<ShoppingItem>,
    pub total_count: usize,
    pub active_count: usize,
    pub completed_count: usize,
}

/// Filter applied when reading the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemStatus {
    All,
    Active,
    Completed,
}

impl ItemStatus {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "active" | "incomplete" | "todo" => Self::Active,
            "completed" | "done" => Self::Completed,
            _ => Self::All,
        }
    }
}

impl ShoppingList {
    fn from_items(list_id: Option<String>, items: Vec<ShoppingItem>) -> Self {
        let active_count = items.iter().filter(|item| !item.completed).count();
        let completed_count = items.len() - active_count;
        Self {
            list_id,
            total_count: items.len(),
            active_count,
            completed_count,
            items,
        }
    }

    /// Items matching the requested status, preserving the list order.
    pub fn filtered(&self, status: ItemStatus) -> Vec<ShoppingItem> {
        self.items
            .iter()
            .filter(|item| match status {
                ItemStatus::All => true,
                ItemStatus::Active => !item.completed,
                ItemStatus::Completed => item.completed,
            })
            .cloned()
            .collect()
    }
}

/// Cookies read from the live browser session.
#[derive(Debug, Clone, Serialize)]
pub struct CookieExport {
    pub count: usize,
    pub saved_to: Option<String>,
    pub cookies: Vec<RawCookie>,
}

/// Client for the reverse-engineered Alexa shopping list API.
pub struct AlexaClient {
    config: AppConfig,
    http: Client,
    browser_client: BrowserClient,
    throttler: Arc<Throttler>,
    list_id: Mutex<Option<String>>,
}

impl AlexaClient {
    pub fn new(config: AppConfig, browser_client: BrowserClient) -> Self {
        let http = Client::builder()
            .user_agent(MOBILE_UA)
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("failed to build the HTTP client");
        let throttler = Arc::new(Throttler::new(
            config.min_interval_ms,
            config.jitter_ms,
            config.max_retries,
            config.backoff_base_ms,
        ));
        Self {
            config,
            http,
            browser_client,
            throttler,
            list_id: Mutex::new(None),
        }
    }

    // ---- Reads -------------------------------------------------------------

    /// Fetches the whole shopping list.
    pub async fn get_shopping_list(&self) -> Result<ShoppingList> {
        let url = format!(
            "{}/alexashoppinglists/api/getlistitems",
            self.config.amazon_origin()
        );
        let value = self.request(Method::GET, &url, None).await?;
        let (list_id, items) = parse_list_response(&value)?;

        if let Some(id) = &list_id {
            *self.list_id.lock().await = Some(id.clone());
        }
        let list = ShoppingList::from_items(list_id, items);
        info!(
            "Alexa shopping list: {} item(s) ({} active, {} completed)",
            list.total_count, list.active_count, list.completed_count
        );
        Ok(list)
    }

    // ---- Mutations ---------------------------------------------------------

    /// Adds an item to the shopping list.
    pub async fn add_item(&self, value: &str) -> Result<()> {
        let value = value.trim();
        if value.is_empty() {
            bail!("The item name must not be empty");
        }
        let list_id = self.resolve_list_id().await?;
        // The endpoint expects the URL-encoded base64 of the list id.
        let encoded = urlencoding::encode(&STANDARD.encode(list_id.as_bytes())).to_string();
        let url = format!(
            "{}/alexashoppinglists/api/addlistitem/{}",
            self.config.amazon_origin(),
            encoded
        );
        let payload = json!({ "value": value, "type": "TASK" });
        self.request(Method::POST, &url, Some(payload)).await?;
        info!("Added '{value}' to the Alexa shopping list");
        Ok(())
    }

    /// Marks (or unmarks) an item as completed. The reference accepts either the
    /// item id or its exact name.
    pub async fn complete_item(&self, item_ref: &str, completed: bool) -> Result<ShoppingItem> {
        let list = self.get_shopping_list().await?;
        let item = find_item(&list.items, item_ref)?;

        let mut payload = serde_json::to_value(item)?;
        if let Some(object) = payload.as_object_mut() {
            object.insert("completed".to_string(), json!(completed));
        }

        let url = format!(
            "{}/alexashoppinglists/api/updatelistitem",
            self.config.amazon_origin()
        );
        self.request(Method::PUT, &url, Some(payload)).await?;
        info!(
            "{} '{}' on the Alexa shopping list",
            if completed { "Completed" } else { "Reopened" },
            item.value
        );
        Ok(item.clone())
    }

    /// Deletes an item from the shopping list.
    pub async fn delete_item(&self, item_ref: &str) -> Result<ShoppingItem> {
        let list = self.get_shopping_list().await?;
        let item = find_item(&list.items, item_ref)?;
        let payload = serde_json::to_value(item)?;

        let url = format!(
            "{}/alexashoppinglists/api/deletelistitem",
            self.config.amazon_origin()
        );
        self.request(Method::DELETE, &url, Some(payload)).await?;
        info!("Deleted '{}' from the Alexa shopping list", item.value);
        Ok(item.clone())
    }

    // ---- Cookies -----------------------------------------------------------

    /// Reads the Amazon cookies from the live browser session, optionally
    /// persisting them in the export format used by config/alexa-cookies.json.
    pub async fn export_cookies(&self, save: bool) -> Result<CookieExport> {
        let origin = self.config.amazon_origin();
        let page = self.browser_client.ensure_page(&origin).await?;
        let cookies = self.browser_client.get_cookies(&page).await?;
        let _ = page.close().await;

        let saved_to = if save {
            let json = serialize_cookies(&cookies)?;
            if let Some(parent) = self.config.cookies_output_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&self.config.cookies_output_path, json).with_context(|| {
                format!(
                    "Could not write cookies to {}",
                    self.config.cookies_output_path.display()
                )
            })?;
            info!(
                "Saved {} Amazon cookies to {}",
                cookies.len(),
                self.config.cookies_output_path.display()
            );
            Some(self.config.cookies_output_path.display().to_string())
        } else {
            None
        };

        Ok(CookieExport {
            count: cookies.len(),
            saved_to,
            cookies,
        })
    }

    // ---- Internals ---------------------------------------------------------

    async fn resolve_list_id(&self) -> Result<String> {
        if let Some(id) = &self.config.alexa_list_id {
            return Ok(id.clone());
        }
        if let Some(id) = self.list_id.lock().await.clone() {
            return Ok(id);
        }
        let list = self.get_shopping_list().await?;
        if let Some(id) = list.list_id {
            return Ok(id);
        }
        bail!("Could not determine the Alexa shopping list id (set ALEXA_LIST_ID)")
    }

    /// Single choke point for every HTTP call: throttled, retried on 429/503,
    /// with cookies and the mobile Alexa headers attached.
    async fn request(&self, method: Method, url: &str, body: Option<Value>) -> Result<Value> {
        self.throttler
            .run(async {
                let mut last_status = 0u16;
                for attempt in 0..=self.throttler.max_retries() {
                    if attempt > 0 {
                        tokio::time::sleep(self.throttler.backoff(attempt)).await;
                    }

                    let mut builder = self
                        .http
                        .request(method.clone(), url)
                        .header("Accept", "*/*")
                        .header("Accept-Language", "*")
                        .header("DNT", "1")
                        .header("Upgrade-Insecure-Requests", "1");
                    if let Some(cookie) =
                        cookie_header(&self.config.alexa_cookies, &self.config.amazon_host())
                    {
                        builder = builder.header("Cookie", cookie);
                    }
                    if let Some(payload) = &body {
                        builder = builder
                            .header("Content-Type", "application/json")
                            .json(payload);
                    }

                    let response = builder
                        .send()
                        .await
                        .with_context(|| format!("Request to {url} failed"))?;
                    let status = response.status();

                    if status == reqwest::StatusCode::UNAUTHORIZED
                        || status == reqwest::StatusCode::FORBIDDEN
                    {
                        bail!(
                            "Amazon rejected the request (HTTP {}). The session cookies are \
                             missing or expired: log into Amazon in the browser and refresh them \
                             with the alexa_get_cookies tool, or set ALEXA_COOKIES_JSON.",
                            status.as_u16()
                        );
                    }

                    if RETRYABLE_STATUSES.contains(&status.as_u16()) {
                        last_status = status.as_u16();
                        warn!("Amazon returned HTTP {last_status}; retrying");
                        continue;
                    }

                    let text = response.text().await.unwrap_or_default();
                    if !status.is_success() {
                        bail!(
                            "Amazon returned HTTP {status}: {}",
                            text.chars().take(300).collect::<String>()
                        );
                    }
                    if text.trim().is_empty() {
                        return Ok(Value::Null);
                    }
                    return serde_json::from_str(&text).with_context(|| {
                        format!("Amazon returned invalid JSON ({} bytes)", text.len())
                    });
                }
                bail!(
                    "Amazon kept returning HTTP {last_status} after {} attempts",
                    self.throttler.max_retries() + 1
                )
            })
            .await
    }
}

/// Parses the getlistitems payload. The API returns an object keyed by the
/// shopping list id, each value carrying a listItems array.
fn parse_list_response(value: &Value) -> Result<(Option<String>, Vec<ShoppingItem>)> {
    if let Some(object) = value.as_object() {
        for (key, entry) in object {
            if let Some(items) = entry.get("listItems").and_then(Value::as_array) {
                return Ok((Some(key.clone()), parse_items(items)?));
            }
        }
    }
    if let Some(items) = value.get("listItems").and_then(Value::as_array) {
        return Ok((None, parse_items(items)?));
    }
    bail!("Unexpected Alexa response: no 'listItems' array found")
}

fn parse_items(items: &[Value]) -> Result<Vec<ShoppingItem>> {
    let mut parsed = Vec::with_capacity(items.len());
    for item in items {
        match serde_json::from_value::<ShoppingItem>(item.clone()) {
            Ok(item) => parsed.push(item),
            Err(error) => warn!("Skipping unparsable shopping item: {error}"),
        }
    }
    Ok(parsed)
}

/// Finds an item by id, then by case-insensitive exact name.
pub fn find_item<'a>(items: &'a [ShoppingItem], reference: &str) -> Result<&'a ShoppingItem> {
    if let Some(item) = items.iter().find(|item| item.id == reference) {
        return Ok(item);
    }
    let needle = reference.trim().to_lowercase();
    items
        .iter()
        .find(|item| item.value.trim().to_lowercase() == needle)
        .with_context(|| format!("No shopping list item matches '{reference}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "amzn1.account.AHDW4I2TM4SR4QT6UJH73VQZZANA-SHOPPING_ITEM": {
        "listItems": [
          {"id": "a1", "value": "Lait demi-écrémé", "completed": false, "version": 3},
          {"id": "b2", "value": "Pain", "completed": true, "version": 1}
        ]
      }
    }"#;

    #[test]
    fn parses_list_and_counts() {
        let value: Value = serde_json::from_str(SAMPLE).unwrap();
        let (list_id, items) = parse_list_response(&value).unwrap();
        assert_eq!(
            list_id.as_deref(),
            Some("amzn1.account.AHDW4I2TM4SR4QT6UJH73VQZZANA-SHOPPING_ITEM")
        );
        let list = ShoppingList::from_items(list_id, items);
        assert_eq!(list.total_count, 2);
        assert_eq!(list.active_count, 1);
        assert_eq!(list.completed_count, 1);
        assert_eq!(list.filtered(ItemStatus::Active).len(), 1);
        assert_eq!(list.filtered(ItemStatus::Completed)[0].value, "Pain");
    }

    #[test]
    fn preserves_unknown_fields_for_round_trip() {
        let value: Value = serde_json::from_str(SAMPLE).unwrap();
        let (_, items) = parse_list_response(&value).unwrap();
        let serialized = serde_json::to_value(&items[0]).unwrap();
        assert_eq!(serialized.get("version"), Some(&json!(3)));
    }

    #[test]
    fn finds_items_by_id_or_name() {
        let value: Value = serde_json::from_str(SAMPLE).unwrap();
        let (_, items) = parse_list_response(&value).unwrap();
        assert_eq!(find_item(&items, "a1").unwrap().value, "Lait demi-écrémé");
        assert_eq!(find_item(&items, "pain").unwrap().id, "b2");
        assert!(find_item(&items, "introuvable").is_err());
    }

    #[test]
    fn treats_unknown_payload_as_error() {
        let value = json!({ "unexpected": true });
        assert!(parse_list_response(&value).is_err());
    }

    #[test]
    fn parses_status_filters() {
        assert_eq!(ItemStatus::parse("active"), ItemStatus::Active);
        assert_eq!(ItemStatus::parse("completed"), ItemStatus::Completed);
        assert_eq!(ItemStatus::parse("whatever"), ItemStatus::All);
    }
}
