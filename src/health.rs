use crate::appstate::{AppState, BackendStatus};
use crate::utils::LockExt;
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Endpoints that may carry a backend's model list, in probe order.
///
/// Ollama serves `/api/tags`; llama.cpp answers that with a 404 and only
/// serves `/v1/models`.
const MODEL_LIST_ENDPOINTS: [&str; 2] = ["/api/tags", "/v1/models"];

/// Parsed model list from `/api/tags` or `/v1/models`.
///
/// `models` is the Ollama shape (also emitted by llama.cpp on `/v1/models`),
/// `data` the OpenAI one. Both default so either may be absent.
#[derive(Deserialize, Clone, Default)]
pub struct ModelsResponse {
    #[serde(default)]
    pub models: Vec<serde_json::Value>,
    #[serde(default)]
    pub data: Vec<serde_json::Value>,
}

impl ModelsResponse {
    /// The richer of the two lists: `models` carries names and details,
    /// `data` usually only an `id`.
    fn entries(self) -> Vec<serde_json::Value> {
        if self.models.is_empty() {
            self.data
        } else {
            self.models
        }
    }
}

/// Why a backend's model list could not be obtained.
pub enum FetchError {
    /// No HTTP response at all — the backend is down.
    Unreachable,
    /// The backend responded, but neither endpoint produced a usable list.
    NoUsableList,
}

/// Fetch a backend's model list, trying `/api/tags` then `/v1/models`.
///
/// `preferred` remembers the endpoint that last worked for this backend so the
/// common case costs one request; it is updated on success and cleared when the
/// remembered endpoint stops working.
pub async fn fetch_backend_models(
    client: &reqwest::Client,
    base_url: &str,
    preferred: &mut Option<&'static str>,
) -> Result<Vec<BackendModelInfo>, FetchError> {
    let mut order: Vec<&'static str> = Vec::with_capacity(MODEL_LIST_ENDPOINTS.len());
    if let Some(p) = *preferred {
        order.push(p);
    }
    order.extend(
        MODEL_LIST_ENDPOINTS
            .iter()
            .filter(|e| Some(**e) != *preferred),
    );

    let mut reachable = false;

    for endpoint in order {
        let url = format!("{}{}", base_url, endpoint);

        let resp = match client.get(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                debug!("Failed to reach {}: {}", url, e);
                continue;
            }
        };

        reachable = true;
        let status = resp.status();

        // The status check is what rejects llama.cpp's 404 error body: with both
        // list fields defaulting, it would otherwise parse as an empty list and
        // be indistinguishable from a backend that genuinely has no models.
        if !status.is_success() {
            debug!("{} returned {}", url, status);
            continue;
        }

        let list = match resp.json::<ModelsResponse>().await {
            Ok(list) => list,
            Err(e) => {
                debug!("Failed to parse model list from {}: {}", url, e);
                continue;
            }
        };

        *preferred = Some(endpoint);

        return Ok(list
            .entries()
            .into_iter()
            .filter_map(
                |value| match serde_json::from_value::<BackendModelInfo>(value.clone()) {
                    Ok(info) if !info.name.is_empty() => Some(info),
                    Ok(_) => {
                        debug!("Ignoring unnamed model entry from {}: {}", url, value);
                        None
                    }
                    Err(e) => {
                        debug!(
                            "Ignoring unparseable model entry from {}: {} ({})",
                            url, e, value
                        );
                        None
                    }
                },
            )
            .collect());
    }

    *preferred = None;

    if reachable {
        Err(FetchError::NoUsableList)
    } else {
        Err(FetchError::Unreachable)
    }
}

/// Full model info from a backend's model list.
///
/// Fields are tolerant on purpose: llama.cpp sends empty strings where Ollama
/// sends numbers, and omits others entirely.
#[derive(Deserialize, Clone)]
#[serde(from = "RawBackendModelInfo")]
pub struct BackendModelInfo {
    pub name: String,
    pub modified_at: String,
    pub size: u64,
    pub digest: String,
    pub details: crate::appstate::ModelDetails,
}

/// Wire shape of a model list entry. Backends may send `name`, `model`, `id`,
/// or several of them (llama.cpp sends both `name` and `model`).
#[derive(Deserialize)]
struct RawBackendModelInfo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// OpenAI `/v1/models` `data[]` entries carry only this.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    modified_at: String,
    #[serde(default, deserialize_with = "flexible_u64")]
    size: u64,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    details: crate::appstate::ModelDetails,
}

impl From<RawBackendModelInfo> for BackendModelInfo {
    fn from(raw: RawBackendModelInfo) -> Self {
        let name = raw
            .name
            .filter(|s| !s.is_empty())
            .or(raw.model.filter(|s| !s.is_empty()))
            .or(raw.id)
            .unwrap_or_default();

        Self {
            name,
            modified_at: raw.modified_at,
            size: raw.size,
            digest: raw.digest,
            details: raw.details,
        }
    }
}

/// Accept a JSON number, a numeric string, an empty string or null for a `u64`.
fn flexible_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(n) => Ok(n.as_u64().unwrap_or(0)),
        serde_json::Value::String(s) => Ok(s.trim().parse::<u64>().unwrap_or(0)),
        _ => Ok(0),
    }
}

/// Build merged /api/tags cache from all configured models using their first backends
pub async fn build_tags_cache(
    state: &AppState,
    client: &reqwest::Client,
    endpoints: &mut HashMap<String, Option<&'static str>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Clone model list to release the lock before awaiting
    let models_to_fetch: Vec<(String, Option<String>, String)> = {
        let config = state.model_config.read().expect("model_config read");
        config
            .models
            .iter()
            .filter(|m| !m.backends.is_empty())
            .map(|m| (m.name.clone(), m.public_name.clone(), m.backends[0].clone()))
            .collect()
    };

    let mut merged_models: Vec<crate::appstate::PublicModelInfo> = Vec::new();

    for (model_name, public_name_opt, backend_url) in models_to_fetch {
        let preferred = endpoints.entry(backend_url.clone()).or_default();

        let backend_models = match fetch_backend_models(client, &backend_url, preferred).await {
            Ok(models) => models,
            Err(_) => {
                debug!(
                    "No model list from {} while caching model {}",
                    backend_url, model_name
                );
                continue;
            }
        };

        // Find matching model in backend response
        if let Some(backend_info) = backend_models
            .into_iter()
            .find(|info| info.name == model_name || info.name.starts_with(model_name.as_str()))
        {
            // Get public_name (or fallback to name)
            let public_name = public_name_opt.as_ref().unwrap_or(&model_name);

            // Create PublicModelInfo with public_name overriding name/model
            merged_models.push(crate::appstate::PublicModelInfo {
                name: public_name.clone(),
                model: public_name.clone(),
                modified_at: backend_info.modified_at,
                size: backend_info.size,
                digest: backend_info.digest,
                details: backend_info.details,
            });
        }
    }

    // Update cache
    {
        let mut cache = state.cached_tags.write().expect("cached_tags write");
        *cache = Some(crate::appstate::CachedTags {
            models: merged_models,
        });
        let model_count = cache.as_ref().map(|c| c.models.len()).unwrap_or(0);
        debug!("Built cache with {} models", model_count);
    }

    Ok(())
}

/// Spawn health checker that monitors backend status every N seconds
pub fn spawn_health_checker(state: Arc<AppState>) {
    tokio::spawn(async move {
        // Remembers which endpoint last served each backend's model list, so
        // the steady state costs one request instead of probing both.
        let mut endpoints: HashMap<String, Option<&'static str>> = HashMap::new();

        // Build initial cache
        let _ = build_tags_cache(&state, &state.client, &mut endpoints).await;

        loop {
            let backends_to_check: Vec<(usize, String)> = {
                let backends = state.backends.lock().lock_unwrap("backends");
                backends
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (i, b.url.clone()))
                    .collect()
            };

            for (idx, url) in backends_to_check {
                let preferred = endpoints.entry(url.clone()).or_default();

                match fetch_backend_models(&state.client, &url, preferred).await {
                    Ok(backend_models) => {
                        // Keep alive all configured models when backend comes back online
                        let backend = {
                            let mut backends = state.backends.lock().lock_unwrap("backends");
                            let backend = &mut backends[idx];

                            if !backend.is_online {
                                info!("Backend {} is back online", url);
                                backend.is_online = true;
                                Some(backend.clone())
                            } else {
                                None
                            }
                        };

                        if let Some(backend) = backend {
                            state.spawn_keep_alive_for_backend(&backend);
                        }

                        // Update per-model status (use base-name matching)
                        let mut backends = state.backends.lock().lock_unwrap("backends");
                        let backend = &mut backends[idx];
                        let mut model_status =
                            backend.model_status.write().expect("model_status write");
                        let backend_model_names: HashSet<String> =
                            backend_models.iter().map(|m| m.name.clone()).collect();

                        for configured_model in &backend.configured_models {
                            let config_base = configured_model
                                .split(':')
                                .next()
                                .unwrap_or(configured_model);
                            let was_available =
                                model_status.get(configured_model).copied().unwrap_or(true);

                            // Check if backend has any model with matching base name
                            let is_available = backend_model_names.iter().any(|backend_model| {
                                let backend_base =
                                    backend_model.split(':').next().unwrap_or(backend_model);
                                backend_base == config_base
                            });

                            if was_available && !is_available {
                                warn!(
                                    "Backend {} no longer has model {} available (but is configured)",
                                    url, configured_model
                                );
                            } else if !was_available && is_available {
                                info!(
                                    "Backend {} now has model {} available",
                                    url, configured_model
                                );
                            }

                            model_status.insert(configured_model.clone(), is_available);
                        }
                    }
                    Err(reason) => {
                        let mut backends = state.backends.lock().lock_unwrap("backends");
                        let backend = &mut backends[idx];

                        if backend.is_online {
                            match reason {
                                FetchError::Unreachable => {
                                    info!("Backend {} went offline", url)
                                }
                                FetchError::NoUsableList => warn!(
                                    "Backend {} responded but served no model list on {} — marking offline",
                                    url,
                                    MODEL_LIST_ENDPOINTS.join(" or ")
                                ),
                            }
                            backend.is_online = false;
                        }

                        // Mark all configured models as unavailable
                        let mut model_status =
                            backend.model_status.write().expect("model_status write");
                        for model_name in &backend.configured_models {
                            model_status.insert(model_name.clone(), false);
                        }
                    }
                }
            }

            // Build merged cache after checking all backends
            let _ = build_tags_cache(&state, &state.client, &mut endpoints).await;

            tokio::time::sleep(std::time::Duration::from_secs(state.health_check_interval)).await;
        }
    });
}

/// Spawn model keeper that triggers keep-alive every 15 minutes
pub fn spawn_model_keeper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let keep_alive_interval = std::time::Duration::from_secs(15 * 60); // 15 minutes

        loop {
            tokio::time::sleep(keep_alive_interval).await;

            state.trigger_all_keep_alives().await;
        }
    });
}

/// Send keep_alive requests for specific models on a backend
pub async fn keep_alive_specific_models(
    backend: &BackendStatus,
    client: &reqwest::Client,
    models: &[String],
    timeout_secs: u64,
) {
    if models.is_empty() {
        return;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .unwrap_or_else(|_| client.clone());

    for model_name in models {
        let model_name = model_name.clone();
        let url = format!("{}/v1/chat/completions", backend.url);
        let body = serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
            "stream": false
        });
        let client = client.clone();

        tokio::spawn(async move {
            match client.post(&url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        debug!("Model {} kept alive on backend {}", model_name, url);
                    } else {
                        warn!(
                            "Failed to keep alive model {} on backend {}: {}",
                            model_name, url, status
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to keep alive model {} on backend {}: {}",
                        model_name, url, e
                    );
                }
            }
        });
    }
}

/// Health check response structure
#[derive(Serialize)]
pub struct HealthResponse {
    pub models: HashMap<String, String>,
}

/// Health check handler - returns status of all models across backends
pub async fn health_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // Simple authentication - no IP tracking, minimal logging
    let valid = match headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        Some(token) => state
            .user_registry
            .lock()
            .lock_unwrap("user_registry")
            .authenticate(token)
            .is_some(),
        None => false,
    };

    if !valid {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    // Build a mapping from internal model name to public_name
    let name_to_public: HashMap<String, String> = {
        let config = state.model_config.read().expect("model_config read");
        config
            .models
            .iter()
            .map(|m| {
                let display_name = m.public_name.clone().unwrap_or_else(|| m.name.clone());
                (m.name.clone(), display_name)
            })
            .collect()
    };

    let mut model_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();

    let backends = state.backends.lock().lock_unwrap("backends");

    for backend in backends.iter() {
        let model_status = backend.model_status.read().expect("model_status read");

        for (model_name, &available) in model_status.iter() {
            let counts = model_counts.entry(model_name.clone()).or_default();
            let key = if available { "up" } else { "down" };
            *counts.entry(key.to_string()).or_insert(0) += 1;
        }
    }

    let mut models: HashMap<String, String> = HashMap::new();
    for (model_name, counts) in model_counts.iter() {
        let up_count = counts.get("up").copied().unwrap_or(0);
        let down_count = counts.get("down").copied().unwrap_or(0);
        let total = up_count + down_count;

        let status = if total == 0 || up_count == 0 {
            "down"
        } else if up_count == total {
            "up"
        } else {
            "degraded"
        };

        // Use public_name if available, otherwise fall back to internal name
        let display_name = name_to_public
            .get(model_name)
            .cloned()
            .unwrap_or_else(|| model_name.clone());

        models.insert(display_name, status.to_string());
    }

    (StatusCode::OK, Json(HealthResponse { models })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_llamacpp_tags_entry() {
        let value = serde_json::json!({
            "name": "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M",
            "model": "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M",
            "modified_at": "",
            "size": "",
            "digest": "",
            "type": "model",
            "capabilities": ["completion", "multimodal"],
            "details": {
                "parent_model": "",
                "format": "gguf",
                "family": "",
                "families": [""],
                "parameter_size": "",
                "quantization_level": ""
            }
        });

        let info: BackendModelInfo = serde_json::from_value(value).expect("llama.cpp entry parses");
        assert_eq!(info.name, "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M");
        assert_eq!(info.size, 0);
        assert_eq!(info.details.format, "gguf");
    }

    #[test]
    fn parses_ollama_tags_entry() {
        let value = serde_json::json!({
            "name": "qwen3:35b",
            "modified_at": "2025-01-01T00:00:00Z",
            "size": 16453443584u64,
            "digest": "abc123",
            "details": {
                "parent_model": "",
                "format": "gguf",
                "family": "qwen3",
                "families": ["qwen3"],
                "parameter_size": "35B",
                "quantization_level": "Q4_K_M"
            }
        });

        let info: BackendModelInfo = serde_json::from_value(value).expect("ollama entry parses");
        assert_eq!(info.name, "qwen3:35b");
        assert_eq!(info.size, 16453443584);
        assert_eq!(info.details.parameter_size, "35B");
    }

    #[test]
    fn parses_minimal_entry_without_details() {
        let value = serde_json::json!({ "model": "some-model:latest" });

        let info: BackendModelInfo = serde_json::from_value(value).expect("minimal entry parses");
        assert_eq!(info.name, "some-model:latest");
        assert_eq!(info.size, 0);
        assert!(info.details.families.is_empty());
    }
}

#[cfg(test)]
mod list_tests {
    use super::*;

    /// The exact `/v1/models` payload llama.cpp serves.
    fn llamacpp_v1_models() -> serde_json::Value {
        serde_json::json!({
            "models": [{
                "name": "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M",
                "model": "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M",
                "modified_at": "",
                "size": "",
                "digest": "",
                "type": "model",
                "description": "",
                "tags": [""],
                "capabilities": ["completion", "multimodal"],
                "parameters": "",
                "details": {
                    "parent_model": "",
                    "format": "gguf",
                    "family": "",
                    "families": [""],
                    "parameter_size": "",
                    "quantization_level": ""
                }
            }],
            "object": "list",
            "data": [{
                "id": "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M",
                "aliases": ["unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M"],
                "tags": [],
                "object": "model",
                "created": 1790195170u64,
                "owned_by": "llamacpp",
                "meta": { "n_ctx": 32000, "size": 16453443584u64 }
            }]
        })
    }

    fn parse(value: serde_json::Value) -> Vec<BackendModelInfo> {
        serde_json::from_value::<ModelsResponse>(value)
            .expect("list parses")
            .entries()
            .into_iter()
            .filter_map(|v| serde_json::from_value::<BackendModelInfo>(v).ok())
            .filter(|i| !i.name.is_empty())
            .collect()
    }

    #[test]
    fn parses_llamacpp_v1_models() {
        let models = parse(llamacpp_v1_models());

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M");
        // `models` is preferred over `data`, so details survive.
        assert_eq!(models[0].details.format, "gguf");
        assert_eq!(models[0].size, 0);
    }

    #[test]
    fn parses_openai_data_only_list() {
        let models = parse(serde_json::json!({
            "object": "list",
            "data": [
                { "id": "qwen3:35b", "object": "model", "owned_by": "library" },
                { "id": "nomic-embed-text:latest", "object": "model" }
            ]
        }));

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "qwen3:35b");
        assert_eq!(models[1].name, "nomic-embed-text:latest");
    }

    #[test]
    fn parses_ollama_api_tags_list() {
        let models = parse(serde_json::json!({
            "models": [{
                "name": "qwen3:35b",
                "modified_at": "2025-01-01T00:00:00Z",
                "size": 16453443584u64,
                "digest": "abc123",
                "details": {
                    "parent_model": "", "format": "gguf", "family": "qwen3",
                    "families": ["qwen3"], "parameter_size": "35B",
                    "quantization_level": "Q4_K_M"
                }
            }]
        }));

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "qwen3:35b");
        assert_eq!(models[0].size, 16453443584);
    }

    /// llama.cpp's 404 body for `/api/tags`. It deserializes happily because
    /// both list fields default, which is exactly why `fetch_backend_models`
    /// gates on the HTTP status before parsing.
    #[test]
    fn error_body_yields_no_models() {
        let models = parse(serde_json::json!({
            "error": { "message": "File Not Found", "type": "not_found_error", "code": 404 }
        }));

        assert!(models.is_empty());
    }

    #[test]
    fn base_name_matching_covers_slashed_model_names() {
        let configured = "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M";
        let models = parse(llamacpp_v1_models());

        let config_base = configured.split(':').next().unwrap();
        let is_available = models.iter().any(|m| {
            let backend_base = m.name.split(':').next().unwrap_or(&m.name);
            backend_base == config_base
        });

        assert!(is_available);
    }
}

#[cfg(test)]
mod fetch_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Minimal stub server: answers each path from a routing table and stops
    /// after `requests` requests. Returns its base URL and a handle yielding
    /// the paths that were actually requested, in order.
    fn stub_backend(
        routes: Vec<(&'static str, u16, &'static str)>,
        requests: usize,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));

        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();

            for _ in 0..requests {
                let (mut sock, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).expect("read");
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();

                let (status, body) = routes
                    .iter()
                    .find(|(p, _, _)| *p == path)
                    .map(|(_, s, b)| (*s, *b))
                    .unwrap_or((404, "{}"));

                let resp = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
                seen.push(path);
            }

            seen
        });

        (base, handle)
    }

    const LLAMACPP_404: &str =
        r#"{"error":{"message":"File Not Found","type":"not_found_error","code":404}}"#;
    const LLAMACPP_MODELS: &str = r#"{"models":[{"name":"unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M","model":"unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M","modified_at":"","size":"","digest":"","details":{"format":"gguf","families":[""]}}],"object":"list","data":[{"id":"unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M"}]}"#;

    #[tokio::test]
    async fn falls_back_to_v1_models_when_api_tags_404s() {
        let (base, server) = stub_backend(
            vec![
                ("/api/tags", 404, LLAMACPP_404),
                ("/v1/models", 200, LLAMACPP_MODELS),
            ],
            3,
        );

        let client = reqwest::Client::new();
        let mut preferred = None;

        let models = fetch_backend_models(&client, &base, &mut preferred)
            .await
            .unwrap_or_else(|_| panic!("should fall back to /v1/models"));

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M");
        assert_eq!(preferred, Some("/v1/models"));

        // The working endpoint is remembered: the next call skips /api/tags.
        let models = fetch_backend_models(&client, &base, &mut preferred)
            .await
            .unwrap_or_else(|_| panic!("second fetch"));
        assert_eq!(models.len(), 1);

        let seen = server.join().expect("server thread");
        assert_eq!(seen, vec!["/api/tags", "/v1/models", "/v1/models"]);
    }

    #[tokio::test]
    async fn prefers_api_tags_for_ollama() {
        let (base, server) = stub_backend(
            vec![(
                "/api/tags",
                200,
                r#"{"models":[{"name":"qwen3:35b","size":123,"digest":"d"}]}"#,
            )],
            1,
        );

        let client = reqwest::Client::new();
        let mut preferred = None;

        let models = fetch_backend_models(&client, &base, &mut preferred)
            .await
            .unwrap_or_else(|_| panic!("ollama fetch"));

        assert_eq!(models[0].name, "qwen3:35b");
        assert_eq!(models[0].size, 123);
        assert_eq!(preferred, Some("/api/tags"));
        // /v1/models is never probed when /api/tags answers.
        assert_eq!(server.join().expect("server thread"), vec!["/api/tags"]);
    }

    #[tokio::test]
    async fn no_usable_list_when_both_endpoints_404() {
        let (base, server) = stub_backend(
            vec![
                ("/api/tags", 404, LLAMACPP_404),
                ("/v1/models", 404, LLAMACPP_404),
            ],
            2,
        );

        let client = reqwest::Client::new();
        let mut preferred = None;

        match fetch_backend_models(&client, &base, &mut preferred).await {
            Err(FetchError::NoUsableList) => {}
            Err(FetchError::Unreachable) => panic!("backend answered, should not be Unreachable"),
            Ok(_) => panic!("404 bodies must not parse as a model list"),
        }

        assert_eq!(preferred, None);
        let _ = server.join();
    }

    #[tokio::test]
    async fn unreachable_backend_is_distinguished() {
        // Bind then drop, so the port is almost certainly closed.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };

        let client = reqwest::Client::new();
        let mut preferred = None;

        match fetch_backend_models(
            &client,
            &format!("http://127.0.0.1:{}", port),
            &mut preferred,
        )
        .await
        {
            Err(FetchError::Unreachable) => {}
            Err(FetchError::NoUsableList) => panic!("nothing answered, should be Unreachable"),
            Ok(_) => panic!("closed port must not yield models"),
        }
    }
}
