use crate::appstate::{AppState, BackendStatus};
use crate::utils::LockExt;
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Parsed response from `/api/tags`
#[derive(Deserialize, Clone)]
pub struct ModelsResponse {
    pub models: Vec<serde_json::Value>,
}

/// Full model info from backend /api/tags response
#[derive(Deserialize, Clone)]
pub struct BackendModelInfo {
    pub name: String,
    pub modified_at: String,
    pub size: u64,
    pub digest: String,
    pub details: crate::appstate::ModelDetails,
}

/// Build merged /api/tags cache from all configured models using their first backends
pub async fn build_tags_cache(
    state: &AppState,
    client: &reqwest::Client,
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
        let url = format!("{}/api/tags", backend_url);

        match client.get(&url).send().await {
            Ok(resp) => {
                if let Ok(backend_response) = resp.json::<ModelsResponse>().await {
                    // Find matching model in backend response
                    if let Some(backend_model) = backend_response.models.iter().find(|value| {
                        if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
                            name == model_name || name.starts_with(model_name.as_str())
                        } else {
                            false
                        }
                    }) {
                        // Parse the backend model with all fields
                        if let Ok(backend_info) =
                            serde_json::from_value::<BackendModelInfo>(backend_model.clone())
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
                }
            }
            Err(e) => {
                debug!(
                    "Failed to fetch /api/tags from {} for model {}: {}",
                    backend_url, model_name, e
                );
            }
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
        // Build initial cache
        let _ = build_tags_cache(&state, &state.client).await;

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
                let check_url = format!("{}/api/tags", url);
                match state.client.get(&check_url).send().await {
                    Ok(resp) => {
                        // Parse full model info from the response
                        let backend_models: Vec<BackendModelInfo> = resp
                            .json::<ModelsResponse>()
                            .await
                            .map(|mr| {
                                mr.models
                                    .into_iter()
                                    .filter_map(|value| {
                                        serde_json::from_value::<BackendModelInfo>(value).ok()
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();

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
                    Err(_) => {
                        let mut backends = state.backends.lock().lock_unwrap("backends");
                        let backend = &mut backends[idx];

                        if backend.is_online {
                            info!("Backend {} went offline", url);
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
            let _ = build_tags_cache(&state, &state.client).await;

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
        let url = format!("{}/api/generate", backend.url);
        let body = serde_json::json!({
            "model": model_name,
            "keep_alive": -1
        });
        let client = client.clone();

        tokio::spawn(async move {
            match client.post(&url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let body = resp.text().await.unwrap_or_default();
                        if body.contains("\"done\":true") || body.contains("\"done_reason\"") {
                            debug!("Model {} kept alive on backend {}", model_name, url);
                        }
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
