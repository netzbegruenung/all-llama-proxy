use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, State},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};

use crate::auth::UserRegistry;

const BLOCKED_FILE: &str = "blocked_items.json";

#[derive(Serialize, Deserialize, Default)]
struct BlockedConfig {
    ips: HashSet<IpAddr>,
    users: HashSet<String>,
}

/// Parsed response from `/api/tags`
#[derive(Deserialize)]
struct ModelsResponse {
    models: Vec<ModelInfo>,
}

#[derive(Deserialize)]
struct ModelInfo {
    name: String,
}

pub enum ResponsePart {
    Status(StatusCode, HeaderMap),
    Chunk(Bytes),
    Error(reqwest::Error),
    #[allow(dead_code)]
    ModelNotFound(String),
}

pub struct Task {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub responder: mpsc::Sender<ResponsePart>,
}

#[derive(Clone)]
pub struct BackendStatus {
    pub url: String,
    pub active_requests: usize,
    pub processed_count: usize,
    pub is_online: bool,
    pub available_models: HashSet<String>,
}

pub struct AppState {
    pub queues: Mutex<HashMap<String, VecDeque<Task>>>,
    pub processing_counts: Mutex<HashMap<String, usize>>,
    pub processed_counts: Mutex<HashMap<String, usize>>,
    pub dropped_counts: Mutex<HashMap<String, usize>>,
    pub user_ips: Mutex<HashMap<String, IpAddr>>,
    pub blocked_ips: Mutex<HashSet<IpAddr>>,
    pub blocked_users: Mutex<HashSet<String>>,
    pub vip_user: Mutex<Option<String>>,
    pub boost_user: Mutex<Option<String>>,
    pub global_counter: Mutex<usize>,
    pub notify: Notify,
    pub backend_freed: Notify,
    pub backends: Mutex<Vec<BackendStatus>>,
    pub last_backend_idx: Mutex<usize>,
    pub timeout: u64,
    /// User registry loaded from `/etc/ollama-mq/users.yaml`.
    /// Wrapped in `Arc` so the SIGHUP handler can atomically swap it.
    pub user_registry: Mutex<Arc<UserRegistry>>,
}

impl AppState {
    pub fn new(ollama_urls: Vec<String>, timeout: u64, registry: UserRegistry) -> Self {
        let (blocked_ips, blocked_users) = Self::load_blocked_items();
        let backends = ollama_urls.into_iter()
            .map(|url| BackendStatus {
                url,
                active_requests: 0,
                processed_count: 0,
                is_online: true, // Default to true until first check
                available_models: HashSet::new(),
            })
            .collect();

        Self {
            queues: Mutex::new(HashMap::new()),
            processing_counts: Mutex::new(HashMap::new()),
            processed_counts: Mutex::new(HashMap::new()),
            dropped_counts: Mutex::new(HashMap::new()),
            user_ips: Mutex::new(HashMap::new()),
            blocked_ips: Mutex::new(blocked_ips),
            blocked_users: Mutex::new(blocked_users),
            vip_user: Mutex::new(None),
            boost_user: Mutex::new(None),
            global_counter: Mutex::new(0),
            notify: Notify::new(),
            backend_freed: Notify::new(),
            backends: Mutex::new(backends),
            last_backend_idx: Mutex::new(0),
            timeout,
            user_registry: Mutex::new(Arc::new(registry)),
        }
    }

    fn load_blocked_items() -> (HashSet<IpAddr>, HashSet<String>) {
        if let Ok(content) = fs::read_to_string(BLOCKED_FILE) {
            if let Ok(config) = serde_json::from_str::<BlockedConfig>(&content) {
                return (config.ips, config.users);
            }
        }
        (HashSet::new(), HashSet::new())
    }

    fn save_blocked_items(&self) {
        let config = BlockedConfig {
            ips: self.blocked_ips.lock().unwrap().clone(),
            users: self.blocked_users.lock().unwrap().clone(),
        };
        if let Ok(content) = serde_json::to_string_pretty(&config) {
            let _ = fs::write(BLOCKED_FILE, content);
        }
    }

    pub fn block_ip(&self, ip: IpAddr) {
        {
            let mut ips = self.blocked_ips.lock().unwrap();
            ips.insert(ip);
        }
        self.save_blocked_items();
        warn!("IP blocked: {}", ip);
    }

    pub fn block_user(&self, user_id: String) {
        {
            let mut users = self.blocked_users.lock().unwrap();
            users.insert(user_id.clone());
        }
        self.save_blocked_items();
        warn!("User blocked: {}", user_id);
    }

    #[allow(dead_code)]
    pub fn unblock_ip(&self, ip: IpAddr) {
        {
            let mut ips = self.blocked_ips.lock().unwrap();
            ips.remove(&ip);
        }
        self.save_blocked_items();
        info!("IP unblocked: {}", ip);
    }

    #[allow(dead_code)]
    pub fn unblock_user(&self, user_id: &str) {
        {
            let mut users = self.blocked_users.lock().unwrap();
            users.remove(user_id);
        }
        self.save_blocked_items();
        info!("User unblocked: {}", user_id);
    }

    pub fn is_ip_blocked(&self, ip: &IpAddr) -> bool {
        self.blocked_ips.lock().unwrap().contains(ip)
    }

    pub fn is_user_blocked(&self, user_id: &str) -> bool {
        self.blocked_users.lock().unwrap().contains(user_id)
    }
}

/// Paths that carry a `model` field in their JSON body.
const MODEL_PATHS: &[&str] = &[
    "/api/generate",
    "/api/chat",
    "/api/embed",
    "/api/embeddings",
    "/v1/chat/completions",
    "/v1/completions",
    "/v1/embeddings",
];

/// Extract the `model` field from the request body when the path is model-aware.
/// Returns `None` for non-model endpoints or when the field is absent.
fn extract_model_name(body: &Bytes, path: &str) -> Option<String> {
    if !MODEL_PATHS.iter().any(|p| path.starts_with(p)) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")?.as_str().map(|s| normalize_model_tag(s))
}

/// Append `:latest` if the model name has no explicit tag.
fn normalize_model_tag(name: &str) -> String {
    if name.contains(':') {
        name.to_string()
    } else {
        format!("{}:latest", name)
    }
}

/// True if `available` contains the requested model.
/// Both sides are normalized to `name:tag` before comparison.
fn backend_has_model(available: &HashSet<String>, requested: &str) -> bool {
    let norm = normalize_model_tag(requested);
    available.iter().any(|m| normalize_model_tag(m) == norm)
}

pub async fn run_worker(state: Arc<AppState>) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(state.timeout))
        .build()
        .unwrap();
    let mut current_idx = 0;

    // Background Health Check
    let health_state = state.clone();
    let health_client = client.clone();
    tokio::spawn(async move {
        loop {
            let backends_to_check: Vec<(usize, String)> = {
                let backends = health_state.backends.lock().unwrap();
                backends.iter().enumerate().map(|(i, b)| (i, b.url.clone())).collect()
            };

            for (idx, url) in backends_to_check {
                let check_url = format!("{}/api/tags", url);
                match health_client.get(&check_url).send().await {
                    Ok(resp) => {
                        // Parse available models from the response body.
                        let models: HashSet<String> = resp
                            .json::<ModelsResponse>()
                            .await
                            .map(|mr| mr.models.into_iter().map(|m| m.name).collect())
                            .unwrap_or_default();

                        let mut backends = health_state.backends.lock().unwrap();
                        if !backends[idx].is_online {
                            info!("Backend {} status changed to: ONLINE", url);
                            backends[idx].is_online = true;
                        }
                        backends[idx].available_models = models;
                    }
                    Err(_) => {
                        let mut backends = health_state.backends.lock().unwrap();
                        if backends[idx].is_online {
                            info!("Backend {} status changed to: OFFLINE", url);
                            backends[idx].is_online = false;
                        }
                        backends[idx].available_models.clear();
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });

    enum SelectionResult {
        Dispatch(String, Task, usize, String),
        ModelNotFound(Task, String),
        Wait,
    }

    loop {
        let selection = {
            let mut queues = state.queues.lock().unwrap();
            let mut backends = state.backends.lock().unwrap();
            let mut last_idx = state.last_backend_idx.lock().unwrap();
            
            // 1. Find all available online backends (Limit: 1 request per backend)
            let online_indices: Vec<usize> = backends.iter()
                .enumerate()
                .filter(|(_, b)| b.is_online && b.active_requests < 1)
                .map(|(i, _)| i)
                .collect();

            if online_indices.is_empty() {
                SelectionResult::Wait
            } else {
                let mut target_user = None;
                let vip = state.vip_user.lock().unwrap().clone();
                let boost = state.boost_user.lock().unwrap().clone();
                let mut counter = state.global_counter.lock().unwrap();

                let mut active_users: Vec<String> = queues.keys()
                    .filter(|u| !queues.get(*u).unwrap().is_empty())
                    .cloned()
                    .collect();

                if active_users.is_empty() {
                    SelectionResult::Wait
                } else {
                    active_users.sort_by(|a, b| {
                        let a_total = state.processed_counts.lock().unwrap().get(a).cloned().unwrap_or(0);
                        let b_total = state.processed_counts.lock().unwrap().get(b).cloned().unwrap_or(0);
                        a_total.cmp(&b_total).then_with(|| a.cmp(b))
                    });

                    if let Some(ref v) = vip { if active_users.contains(v) { target_user = Some(v.clone()); } }
                    if target_user.is_none() {
                        if let Some(ref b) = boost {
                            if active_users.contains(b) && *counter % 2 == 0 { target_user = Some(b.clone()); }
                        }
                    }
                    if target_user.is_none() {
                        if current_idx >= active_users.len() { current_idx = 0; }
                        target_user = Some(active_users[current_idx].clone());
                        current_idx += 1;
                    }

                    match target_user {
                        Some(user_id) => {
                            let task = queues.get_mut(&user_id).unwrap().pop_front().unwrap();
                            *counter += 1;

                            // Filter backends by model availability.
                            let model_opt = extract_model_name(&task.body, &task.path);
                            let eligible: Vec<usize> = if let Some(ref model) = model_opt {
                                online_indices.iter().cloned()
                                    .filter(|&i| backend_has_model(&backends[i].available_models, model))
                                    .collect()
                            } else {
                                online_indices.clone()
                            };

                            if eligible.is_empty() {
                                let model_name = model_opt.unwrap_or_default();
                                SelectionResult::ModelNotFound(task, model_name)
                            } else {
                                // Round-Robin among eligible backends with min connections
                                let min_conns = eligible.iter().map(|&i| backends[i].active_requests).min().unwrap();
                                let candidates: Vec<usize> = eligible.iter().cloned().filter(|&i| backends[i].active_requests == min_conns).collect();
                                let candidate_pos = candidates.iter().position(|&i| i > *last_idx).unwrap_or(0);
                                let selected_backend_idx = candidates[candidate_pos];

                                *last_idx = selected_backend_idx;
                                backends[selected_backend_idx].active_requests += 1;

                                SelectionResult::Dispatch(user_id, task, selected_backend_idx, backends[selected_backend_idx].url.clone())
                            }
                        }
                        None => SelectionResult::Wait,
                    }
                }
            }
        };

        match selection {
            SelectionResult::ModelNotFound(task, model_name) => {
                warn!("No backend has model '{}', returning 503", model_name);
                let error_body = Bytes::from(
                    serde_json::json!({"error": format!("Model '{}' not available on any backend", model_name)}).to_string()
                );
                let mut headers = HeaderMap::new();
                headers.insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                );
                let _ = task.responder.send(ResponsePart::Status(StatusCode::SERVICE_UNAVAILABLE, headers)).await;
                let _ = task.responder.send(ResponsePart::Chunk(error_body)).await;
            }
            SelectionResult::Dispatch(user_id, task, backend_idx, backend_url) => {
                let state_clone = state.clone();
                let client_clone = client.clone();
                let url = format!("{}{}", backend_url, task.path);

                tokio::spawn(async move {
                    let is_blocked = {
                        let user_ips = state_clone.user_ips.lock().unwrap();
                        let blocked_ips = state_clone.blocked_ips.lock().unwrap();
                        let blocked_users = state_clone.blocked_users.lock().unwrap();
                        blocked_users.contains(&user_id) || user_ips.get(&user_id).map(|ip| blocked_ips.contains(ip)).unwrap_or(false)
                    };

                    if is_blocked || task.responder.is_closed() {
                        let mut dropped = state_clone.dropped_counts.lock().unwrap();
                        *dropped.entry(user_id.clone()).or_insert(0) += 1;
                    } else {
                        {
                            let mut processing = state_clone.processing_counts.lock().unwrap();
                            *processing.entry(user_id.clone()).or_insert(0) += 1;
                        }

                        let res_fut = client_clone.request(task.method, &url)
                            .headers(task.headers)
                            .body(task.body)
                            .send();

                        match res_fut.await {
                            Ok(response) => {
                                let status = response.status();
                                let mut headers = response.headers().clone();
                                headers.remove(axum::http::header::TRANSFER_ENCODING);
                                headers.remove(axum::http::header::CONTENT_LENGTH);

                                if task.responder.send(ResponsePart::Status(status, headers)).await.is_ok() {
                                    let mut stream = response.bytes_stream();
                                    let mut client_disconnected = false;
                                    while let Some(chunk_res) = stream.next().await {
                                        match chunk_res {
                                            Ok(chunk) => {
                                                if task.responder.send(ResponsePart::Chunk(chunk)).await.is_err() {
                                                    client_disconnected = true;
                                                    break;
                                                }
                                            }
                                            Err(_) => break,
                                        }
                                    }

                                    if !client_disconnected {
                                        let mut counts = state_clone.processed_counts.lock().unwrap();
                                        *counts.entry(user_id.clone()).or_insert(0) += 1;
                                    } else {
                                        let mut dropped = state_clone.dropped_counts.lock().unwrap();
                                        *dropped.entry(user_id.clone()).or_insert(0) += 1;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = task.responder.send(ResponsePart::Error(e)).await;
                                let mut dropped = state_clone.dropped_counts.lock().unwrap();
                                *dropped.entry(user_id.clone()).or_insert(0) += 1;
                            }
                        }

                        {
                            let mut processing = state_clone.processing_counts.lock().unwrap();
                            if let Some(count) = processing.get_mut(&user_id) { *count = count.saturating_sub(1); }
                        }
                    }

                    {
                        let mut backends = state_clone.backends.lock().unwrap();
                        backends[backend_idx].active_requests = backends[backend_idx].active_requests.saturating_sub(1);
                        backends[backend_idx].processed_count += 1;
                    }
                    state_clone.backend_freed.notify_one();
                });
            }
            SelectionResult::Wait => {
                tokio::select! {
                    _ = state.notify.notified() => {},
                    _ = state.backend_freed.notified() => {},
                }
            }
        }
    }
}

pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    method: Method,
    headers: HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    body: Bytes,
) -> impl IntoResponse {
    let path = uri.path().to_string();
    let ip = addr.ip();

    // --- Authentication ---
    // Extract and validate the Bearer token from the Authorization header.
    let raw_token = match headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        Some(token) => token.to_string(),
        None => {
            warn!("Rejected request from {}: missing or malformed Authorization header", ip);
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    };

    let user_id = {
        let registry = state.user_registry.lock().unwrap().clone();
        match registry.authenticate(&raw_token) {
            Some(uid) => uid.to_string(),
            None => {
                warn!("Rejected request from {}: invalid token", ip);
                return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
            }
        }
    };

    if state.is_ip_blocked(&ip) {
        warn!("Blocked request from IP: {} for user: {}", ip, user_id);
        return (StatusCode::FORBIDDEN, "IP blocked").into_response();
    }

    if state.is_user_blocked(&user_id) {
        warn!("Blocked request from user: {} (IP: {})", user_id, ip);
        return (StatusCode::FORBIDDEN, "User blocked").into_response();
    }

    {
        let mut ips = state.user_ips.lock().unwrap();
        ips.insert(user_id.clone(), ip);
    }

    let (tx, rx) = mpsc::channel(32);
    let mut task_headers = headers.clone();
    task_headers.remove(axum::http::header::HOST);

    let task = Task {
        path,
        method,
        headers: task_headers,
        responder: tx,
        body,
    };

    {
        let mut queues = state.queues.lock().unwrap();
        queues
            .entry(user_id.clone())
            .or_insert_with(VecDeque::new)
            .push_back(task);
    }

    state.notify.notify_one();

    let mut rx = rx;
    match rx.recv().await {
        Some(ResponsePart::Status(status, headers)) => {
            let stream = ReceiverStream::new(rx).map(|part| {
                match part {
                    ResponsePart::Chunk(chunk) => Ok(chunk),
                    ResponsePart::Error(e) => Err(e),
                    ResponsePart::ModelNotFound(_) | ResponsePart::Status(_, _) => Ok(Bytes::new()),
                }
            });

            let mut res = Body::from_stream(stream).into_response();
            *res.status_mut() = status;
            *res.headers_mut() = headers;
            res
        }
        Some(ResponsePart::Error(e)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Backend error: {}", e)).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "Worker failed to respond").into_response(),
    }
}
