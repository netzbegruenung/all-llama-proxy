use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, State, Path},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    fs,
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, RwLock},
    time::Instant,
};
use tokio::sync::{mpsc, Notify};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn, debug, error};

use crate::auth::UserRegistry;

const BLOCKED_FILE: &str = "blocked_items.json";

/// Format duration showing only seconds if < 1 minute, otherwise minutes and seconds
fn format_duration_short(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else {
        let mins = secs / 60;
        let remaining_secs = secs % 60;
        format!("{}m {}s", mins, remaining_secs)
    }
}

/// Extract client IP from headers or fallback to connection address
fn extract_client_ip(
    headers: &HeaderMap,
    addr: SocketAddr,
    ip_header: &Option<String>,
) -> IpAddr {
    if let Some(header_name) = ip_header {
        headers
            .get(header_name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next().and_then(|ip| ip.trim().parse().ok()))
            .unwrap_or_else(|| addr.ip())
    } else {
        addr.ip()
    }
}

/// Authenticate request and return user ID if successful
fn authenticate_request(
    headers: &HeaderMap,
    user_registry: &Mutex<Arc<UserRegistry>>,
    ip: IpAddr,
    is_debug: bool,
) -> Option<String> {
    let raw_token = match headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        Some(token) => token,
        None => {
            if is_debug {
                debug!("Rejected request from {}: missing or malformed Authorization header", ip);
            } else {
                warn!("Rejected request from {}: missing or malformed Authorization header", ip);
            }
            return None;
        }
    };

    match user_registry.lock().unwrap().authenticate(raw_token) {
        Some(uid) => {
            if is_debug {
                debug!("Authenticated user: {} from IP: {}", uid, ip);
            }
            Some(uid.to_string())
        }
        None => {
            if is_debug {
                debug!("Rejected request from {}: invalid token", ip);
            } else {
                warn!("Rejected request from {}: invalid token", ip);
            }
            None
        }
    }
}

/// Log entry for the in-TUI log buffer
pub struct LogEntry {
    pub level: String,
    pub message: String,
}

/// Thread-safe circular buffer for log messages
#[derive(Clone)]
pub struct LogBuffer {
    lines: Arc<RwLock<VecDeque<LogEntry>>>,
    max_lines: usize,
}

impl LogBuffer {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: Arc::new(RwLock::new(VecDeque::with_capacity(max_lines))),
            max_lines,
        }
    }

    pub fn append(&self, level: &str, message: String) {
        if let Ok(mut lines) = self.lines.write() {
            lines.push_back(LogEntry {
                level: level.to_string(),
                message,
            });
            // Keep only the last max_lines entries
            while lines.len() > self.max_lines {
                lines.pop_front();
            }
        }
    }

    pub fn get_last_n(&self, n: usize) -> Vec<(String, String)> {
        if let Ok(lines) = self.lines.read() {
            let total = lines.len();
            lines.iter()
                .skip(total.saturating_sub(n))
                .map(|entry| (entry.level.clone(), entry.message.clone()))
                .collect()
        } else {
            Vec::new()
        }
    }
}

/// Writer that captures tracing output into a LogBuffer
pub struct LogBufferWriter {
    buffer: LogBuffer,
    current_line: String,
}

impl LogBufferWriter {
    pub fn new(buffer: LogBuffer) -> Self {
        Self {
            buffer,
            current_line: String::new(),
        }
    }

    fn parse_and_store(&mut self, line: &str) {
        // Parse format: "LEVEL: message" or extract level from line
        let (level, message) = if line.starts_with("DEBUG") {
            ("DEBUG", line[6..].trim())
        } else if line.starts_with("INFO") {
            ("INFO", line[5..].trim())
        } else if line.starts_with("WARN") {
            ("WARN", line[5..].trim())
        } else if line.starts_with("ERROR") {
            ("ERROR", line[6..].trim())
        } else {
            ("INFO", line)
        };

        self.buffer.append(level, message.to_string());
    }
}

impl Write for LogBufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(line) = std::str::from_utf8(buf) {
            self.current_line.push_str(line);
            
            // Process complete lines (ending with newline)
            while let Some(pos) = self.current_line.find('\n') {
                let complete_line = self.current_line[..pos].trim().to_string();
                if !complete_line.is_empty() {
                    self.parse_and_store(&complete_line);
                }
                self.current_line = self.current_line[pos + 1..].to_string();
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Process any remaining line on flush
        if !self.current_line.is_empty() {
            let complete_line = self.current_line.trim().to_string();
            if !complete_line.is_empty() {
                self.parse_and_store(&complete_line);
            }
            self.current_line.clear();
        }
        Ok(())
    }
}

/// MakeWriter implementation for LogBufferWriter
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBufferWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        LogBufferWriter {
            buffer: self.buffer.clone(),
            current_line: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct BlockedConfig {
    ips: HashSet<IpAddr>,
    users: HashSet<String>,
}

/// Parsed response from `/api/tags`
#[derive(Deserialize, Clone)]
struct ModelsResponse {
    models: Vec<serde_json::Value>,
}

#[derive(Deserialize, Clone)]
#[allow(dead_code)]
struct ModelInfo {
    name: String,
}

/// OpenAI-compatible model response structure
#[derive(Serialize)]
pub(crate) struct OpenAIModel {
    pub(crate) id: String,
    pub(crate) object: &'static str,
    pub(crate) created: i64,
    pub(crate) owned_by: String,
}

#[derive(Serialize)]
pub(crate) struct OpenAIModelsList {
    pub(crate) object: &'static str,
    pub(crate) data: Vec<OpenAIModel>,
}

// Full model info from backend /api/tags response
#[derive(Deserialize, Clone)]
#[allow(dead_code)]
struct BackendModelInfo {
    name: String,
    model: String,
    modified_at: String,
    size: u64,
    digest: String,
    details: ModelDetails,
}

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct ModelDetails {
    pub(crate) parent_model: String,
    pub(crate) format: String,
    pub(crate) family: String,
    pub(crate) families: Vec<String>,
    pub(crate) parameter_size: String,
    pub(crate) quantization_level: String,
}

// Public-facing model info with public_name substitution
#[derive(Serialize, Clone)]
pub(crate) struct PublicModelInfo {
    pub(crate) name: String,
    pub(crate) model: String,
    pub(crate) modified_at: String,
    pub(crate) size: u64,
    pub(crate) digest: String,
    pub(crate) details: ModelDetails,
}

// Cached tags response
#[derive(Clone, Serialize)]
pub(crate) struct CachedTags {
    pub(crate) models: Vec<PublicModelInfo>,
}

/// Loaded from models.yaml - explicit model-to-backend mapping
#[derive(Deserialize, Clone)]
pub struct ModelConfig {
    pub models: Vec<ParsedModel>,
}

/// Individual model configuration with explicit backends and aliases
#[derive(Deserialize, Clone)]
pub struct ParsedModel {
    /// Real model name (required, used for routing)
    pub name: String,
    
    /// Display name for /api/tags and TUI (optional, defaults to name)
    #[serde(default)]
    pub public_name: Option<String>,
    
    /// Backend URLs that serve this model
    pub backends: Vec<String>,
    
    /// Alias names that resolve to this model
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Runtime backend status
#[derive(Clone)]
pub struct BackendStatus {
    pub url: String,
    pub active_requests: usize,
    pub processed_count: usize,
    pub is_online: bool,
    /// Models this backend is configured to serve (from config)
    pub configured_models: Vec<String>,
    /// Per-model verification status (true = available, false = not available)
    pub model_status: Arc<RwLock<HashMap<String, bool>>>,
}

impl BackendStatus {
    pub fn new(url: String) -> Self {
        Self {
            url,
            active_requests: 0,
            processed_count: 0,
            is_online: true,
            configured_models: Vec::new(),
            model_status: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// Check if this backend can serve a specific model
    pub fn can_serve_model(&self, model_name: &str) -> bool {
        // Extract base model name (without tag) for flexible matching
        let request_base = model_name.split(':').next().unwrap_or(model_name);
        
        // Check if any configured model matches (by base name or exact)
        let has_matching_config = self.configured_models.iter().any(|configured| {
            let configured_base = configured.split(':').next().unwrap_or(configured);
            configured == model_name || configured_base == request_base
        });
        
        if !has_matching_config {
            return false;
        }
        
        // Check per-model verification status using base-name matching
        self.model_status
            .read()
            .map(|status| {
                status.get(model_name)
                    .copied()
                    .or_else(|| {
                        status.iter()
                            .find(|(k, _)| {
                                let configured_base = k.split(':').next().unwrap_or(k);
                                configured_base == request_base
                            })
                            .map(|(_, v)| *v)
                    })
                    .unwrap_or(true)
            })
            .unwrap_or(true)
    }
}

impl ModelConfig {
    /// Load model configuration from YAML file
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string(path)?;
        let config: ModelConfig = serde_yaml::from_str(&content)?;
        
        // Validate that each model has at least one backend
        for model in &config.models {
            if model.backends.is_empty() {
                return Err(format!("Model '{}' has no backends configured", model.name).into());
            }
        }
        
        Ok(config)
    }
    
    /// Resolve alias to real model name, returns None if not found
    pub fn resolve_alias(&self, model_name: &str) -> Option<String> {
        // Check aliases first
        for model in &self.models {
            if model.aliases.contains(&model_name.to_string()) {
                return Some(model.name.clone());
            }
        }
        
        // Check if it's already a real model name
        for model in &self.models {
            if model.name == model_name {
                return Some(model.name.clone());
            }
        }
        
        None
    }
    
    /// Get ParsedModel by name or alias
    pub fn get_model(&self, model_name: &str) -> Option<&ParsedModel> {
        self.models.iter().find(|m| {
            m.name == model_name || m.aliases.contains(&model_name.to_string())
        })
    }
    
    /// Get all unique backend URLs from all models
    pub fn get_all_backends(&self) -> BTreeSet<String> {
        let mut backends = BTreeSet::new();
        for model in &self.models {
            for backend in &model.backends {
                backends.insert(backend.clone());
            }
        }
        backends
    }
    
    /// Get models configured for a specific backend
    pub fn get_models_for_backend(&self, backend_url: &str) -> Vec<String> {
        self.models
            .iter()
            .filter(|m| m.backends.contains(&backend_url.to_string()))
            .map(|m| m.name.clone())
            .collect()
    }
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
    pub resolved_model: Option<String>,
}



pub struct AppState {
    pub queues: Mutex<HashMap<String, VecDeque<Task>>>,
    pub processing_counts: Mutex<HashMap<String, usize>>,
    pub processed_counts: Mutex<HashMap<String, usize>>,
    pub dropped_counts: Mutex<HashMap<String, usize>>,
    pub user_ips: Mutex<HashMap<String, IpAddr>>,
    pub blocked_ips: Mutex<HashSet<IpAddr>>,
    pub blocked_users: Mutex<HashSet<String>>,
    pub vip_user: Mutex<Vec<String>>,
    pub boost_user: Mutex<Option<String>>,
    pub global_counter: Mutex<usize>,
    pub notify: Notify,
    pub backend_freed: Notify,
    pub backends: Mutex<Vec<BackendStatus>>,
    pub last_backend_idx: Mutex<usize>,
    pub timeout: u64,
    /// User registry loaded from users.yaml
    pub user_registry: Mutex<Arc<UserRegistry>>,
    /// Model configuration: real models with backends and aliases
    pub model_config: Arc<RwLock<ModelConfig>>,
    /// Debug mode: enable verbose logging
    pub debug: bool,
    /// Shared log buffer for in-TUI logging
    pub log_buffer: LogBuffer,
    /// HTTP header to extract real client IP from
    pub ip_header: Option<String>,
    /// Cached /api/tags response with merged models
    pub cached_tags: Arc<RwLock<Option<CachedTags>>>,
    /// Health check interval in seconds
    pub health_check_interval: u64,
}

impl AppState {
    pub fn new(
        model_config_path: String,
        timeout: u64,
        registry: UserRegistry,
        debug: bool,
        log_buffer: LogBuffer,
        ip_header: Option<String>,
        health_check_interval: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (blocked_ips, blocked_users) = Self::load_blocked_items();
        
        // Load model configuration (required, will crash if missing)
        let model_config = ModelConfig::load(&model_config_path)?;
        info!("Loaded model configuration from {}", model_config_path);
        
        // Build deduplicated backend list from config
        let backend_urls = model_config.get_all_backends();
        
        // Create BackendStatus entries with model configuration
        let mut backends: Vec<BackendStatus> = backend_urls.into_iter()
            .map(|url| {
                let mut backend = BackendStatus::new(url.clone());
                backend.configured_models = model_config.get_models_for_backend(&url);
                
                // Initialize model_status to true for all configured models
                // (optimistic: assume available until health check proves otherwise)
                let mut initial_status = HashMap::new();
                for model_name in &backend.configured_models {
                    initial_status.insert(model_name.clone(), true);
                }
                *backend.model_status.write().unwrap() = initial_status;
                
                backend
            })
            .collect();
        
        // Sort backends for consistent ordering (optional, for TUI display)
        backends.sort_by(|a, b| a.url.cmp(&b.url));
        
        info!("{} backends configured from model config", backends.len());
        for backend in &backends {
            info!("  Backend {}: serves {} models", 
                backend.url, 
                backend.configured_models.len()
            );
        }

        Ok(Self {
            queues: Mutex::new(HashMap::new()),
            processing_counts: Mutex::new(HashMap::new()),
            processed_counts: Mutex::new(HashMap::new()),
            dropped_counts: Mutex::new(HashMap::new()),
            user_ips: Mutex::new(HashMap::new()),
            blocked_ips: Mutex::new(blocked_ips),
            blocked_users: Mutex::new(blocked_users),
            vip_user: Mutex::new(registry.get_vip_users()),
            boost_user: Mutex::new(None),
            global_counter: Mutex::new(0),
            notify: Notify::new(),
            backend_freed: Notify::new(),
            backends: Mutex::new(backends),
            last_backend_idx: Mutex::new(0),
            timeout,
            user_registry: Mutex::new(Arc::new(registry)),
            model_config: Arc::new(RwLock::new(model_config)),
            debug,
            log_buffer,
            ip_header,
            cached_tags: Arc::new(RwLock::new(None)),
            health_check_interval,
        })
    }
    
    /// Reload model configuration (called from SIGHUP handler)
    pub fn reload_model_config(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let new_config = ModelConfig::load(path)?;
        
        // Update the config
        {
            let mut write_guard = self.model_config.write().unwrap();
            *write_guard = new_config.clone();
        }
        
        // Update backend configurations
        {
            let mut backends = self.backends.lock().unwrap();
            for backend in backends.iter_mut() {
                backend.configured_models = new_config.get_models_for_backend(&backend.url);
                
                // Preserve existing per-model status for models that remain configured
                let mut new_status = HashMap::new();
                for model_name in &backend.configured_models {
                    // Check old status, default to true if not known
                    let old_status = self.model_config.read().unwrap()
                        .get_model(model_name)
                        .map(|_| {
                            // Try to get old status
                            backend.model_status.read().unwrap()
                                .get(model_name).copied()
                        });
                    
                    new_status.insert(model_name.clone(), old_status.flatten().unwrap_or(true));
                }
                *backend.model_status.write().unwrap() = new_status;
            }
        }
        
        Ok(())
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

/// Normalize proxy paths to Ollama's API paths
/// Maps `/chat/completions` → `/v1/chat/completions` for backend compatibility
fn normalize_path(path: &str) -> &str {
    match path {
        "/chat/completions" => "/v1/chat/completions",
        _ => path,
    }
}

/// Paths that carry a `model` field in their JSON body.
const MODEL_PATHS: &[&str] = &[
    "/api/generate",
    "/api/chat",
    "/api/embed",
    "/api/embeddings",
    "/chat/completions",
    "/v1/chat/completions",
    "/v1/completions",
    "/v1/embeddings",
    "/v1/responses",
    "/v1/images/generations",
];

/// Extract and resolve the `model` field from the request body when the path is model-aware.
/// If the model is an alias, it's replaced with the real name in the body.
/// Returns `None` for non-model endpoints or when the field is absent.
/// Returns `Some(model_name)` even if not in config (will be 503'd later).
fn extract_and_resolve_model(
    body: &mut Bytes,
    path: &str,
    config: &ModelConfig,
    debug_enabled: bool,
) -> Option<String> {
    if !MODEL_PATHS.iter().any(|p| path.starts_with(p)) {
        if debug_enabled { debug!("Path {} is not a model path", path); }
        return None;
    }

    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let requested_model = v.get("model")?.as_str()?.to_string();
    
    if debug_enabled {
        debug!("Requested model: {} on path {}", requested_model, path);
    }

    // Resolve alias to real model name
    let real_model = match config.resolve_alias(&requested_model) {
        Some(resolved) => {
            if debug_enabled {
                debug!("Resolved {} -> {}", requested_model, resolved);
            }
            resolved
        }
        None => {
            // Not in config at all - don't modify body, will 503 later
            if debug_enabled {
                debug!("Model {} not found in config", requested_model);
            }
            return Some(normalize_model_tag(&requested_model));
        }
    };

    // Update body with real model name
    if real_model != requested_model {
        if let Some(obj) = v.as_object_mut() {
            obj.insert("model".to_string(), serde_json::json!(real_model));
            *body = Bytes::from(serde_json::to_vec(&v).ok()?);
        }
    }

    Some(normalize_model_tag(&real_model))
}

/// Peek at the model name from a request body without modifying it.
/// Used for routing decisions before actually consuming the task.
fn peek_model_from_body(body: &Bytes) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let requested_model = v.get("model")?.as_str()?.to_string();
    Some(requested_model)
}

/// Append `:latest` if the model name has no explicit tag.
fn normalize_model_tag(name: &str) -> String {
    if name.contains(':') {
        name.to_string()
    } else {
        format!("{}:latest", name)
    }
}

/// Parse Unix timestamp from Ollama modified_at format or return current time
fn parse_created_timestamp(modified_at: &str) -> i64 {
    // Try to parse ISO 8601 format from Ollama
    // Modified_at typically looks like: "2024-01-15T10:30:00Z" or similar
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(modified_at) {
        dt.timestamp()
    } else if let Ok(dt) = chrono::DateTime::parse_from_str(modified_at, "%Y-%m-%d %H:%M:%S.%f%#z") {
        dt.timestamp()
    } else {
        // Fallback to current time if parsing fails
        chrono::Utc::now().timestamp()
    }
}

/// Transform cached tags into OpenAI-compatible models list
fn build_models_list(cached_tags: &Option<CachedTags>) -> OpenAIModelsList {
    let models = match cached_tags {
        Some(tags) => tags.models.iter().map(|model| {
            let created = parse_created_timestamp(&model.modified_at);
            
            OpenAIModel {
                id: model.name.clone(),
                object: "model",
                created,
                owned_by: "all-llama-proxy".to_string(),
            }
        }).collect(),
        None => vec![],
    };
    
    OpenAIModelsList {
        object: "list",
        data: models,
    }
}

/// Find a single model by name (matches public_name, name, or alias)
fn find_model_by_name(state: &AppState, requested_model: &str) -> Option<OpenAIModel> {
    let cached_tags = state.cached_tags.read().unwrap();
    
    // First, try to find in cache by name
    let model_info = cached_tags.as_ref().and_then(|tags| {
        tags.models.iter().find(|m| {
            m.name == requested_model || m.name == format!("{}:latest", requested_model)
        })
    });
    
    if let Some(model) = model_info {
        let created = parse_created_timestamp(&model.modified_at);
        return Some(OpenAIModel {
            id: model.name.clone(),
            object: "model",
            created,
            owned_by: "all-llama-proxy".to_string(),
        });
    }
    
    // Also check if it's an alias that resolves to a real model
    let config = state.model_config.read().unwrap();
    if let Some(real_model) = config.resolve_alias(requested_model) {
        // Find the real model in cache
        if let Some(real_model_info) = cached_tags.as_ref().and_then(|tags| {
            tags.models.iter().find(|m| m.name == real_model)
        }) {
            let created = parse_created_timestamp(&real_model_info.modified_at);
            return Some(OpenAIModel {
                id: real_model_info.name.clone(),
                object: "model",
                created,
                owned_by: "all-llama-proxy".to_string(),
            });
        }
    }
    
    None
}

/// Build merged /api/tags cache from all configured models using their first backends
async fn build_tags_cache(state: &AppState, client: &reqwest::Client) -> Result<(), Box<dyn std::error::Error>> {
    // Clone model list to release the lock before awaiting
    let models_to_fetch: Vec<(String, Option<String>, String)> = {
        let config = state.model_config.read().unwrap();
        config.models.iter()
            .filter(|m| !m.backends.is_empty())
            .map(|m| (m.name.clone(), m.public_name.clone(), m.backends[0].clone()))
            .collect()
    };
    
    let mut merged_models: Vec<PublicModelInfo> = Vec::new();

    for (model_name, public_name_opt, backend_url) in models_to_fetch {
        let url = format!("{}/api/tags", backend_url);

        match client.get(&url).send().await {
            Ok(resp) => {
                if let Ok(backend_response) = resp.json::<ModelsResponse>().await {
                    // Find matching model in backend response
                    if let Some(backend_model) = backend_response.models.iter()
                        .find(|value| {
                            if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
                                name == model_name || name.starts_with(model_name.as_str())
                            } else {
                                false
                            }
                        })
                    {
                        // Parse the backend model with all fields
                        if let Ok(backend_info) = serde_json::from_value::<BackendModelInfo>(backend_model.clone()) {
                            // Get public_name (or fallback to name)
                            let public_name = public_name_opt.as_ref()
                                .unwrap_or(&model_name);

                            // Create PublicModelInfo with public_name overriding name/model
                            merged_models.push(PublicModelInfo {
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
                debug!("Failed to fetch /api/tags from {} for model {}: {}", backend_url, model_name, e);
            }
        }
    }

    // Update cache
    {
        let mut cache = state.cached_tags.write().unwrap();
        *cache = Some(CachedTags { models: merged_models });
        debug!("Built cache with {} models", cache.as_ref().unwrap().models.len());
    }

    Ok(())
}

enum SelectionResult {
    Dispatch(String, Task, usize, String),
    ModelNotFound(Task, String),
    Wait,
}

struct DispatchTaskArgs {
    user_id: String,
    task: Task,
    backend_idx: usize,
    backend_url: String,
    state: Arc<AppState>,
    client: reqwest::Client,
}

fn dispatch_task(args: DispatchTaskArgs) {
    let DispatchTaskArgs { user_id, task, backend_idx, backend_url, state, client } = args;
    let url = format!("{}{}", backend_url, task.path);

    if state.debug {
        debug!("Spawning task for user {} -> backend {} ({})",
            user_id, backend_url, task.path);
    }

    tokio::spawn(async move {
        let start = Instant::now();

        let is_blocked = {
            let user_ips = state.user_ips.lock().unwrap();
            let blocked_ips = state.blocked_ips.lock().unwrap();
            let blocked_users = state.blocked_users.lock().unwrap();
            blocked_users.contains(&user_id) || user_ips.get(&user_id).map(|ip| blocked_ips.contains(ip)).unwrap_or(false)
        };

        if is_blocked || task.responder.is_closed() {
            let mut dropped = state.dropped_counts.lock().unwrap();
            *dropped.entry(user_id.clone()).or_insert(0) += 1;
        } else {
            {
                let mut processing = state.processing_counts.lock().unwrap();
                *processing.entry(user_id.clone()).or_insert(0) += 1;
            }

            if state.debug {
                debug!("=== BACKEND REQUEST ===");
                debug!("URL: {}", url);
                debug!("Method: {:?}", task.method);
                debug!("Headers: {:?}", task.headers);
                if let Ok(body_str) = std::str::from_utf8(&task.body) {
                    debug!("Body: {}", body_str);
                } else {
                    debug!("Body: <binary data> {} bytes", task.body.len());
                }
            }

            let res_fut = client.request(task.method, &url)
                .headers(task.headers)
                .body(task.body)
                .send();

            match res_fut.await {
                Ok(response) => {
                    let status = response.status();

                    if state.debug {
                        debug!("Backend {} responded with status {} for user {}",
                            backend_url, status, user_id);
                    }

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
                            let mut counts = state.processed_counts.lock().unwrap();
                            *counts.entry(user_id.clone()).or_insert(0) += 1;

                            // Log completion
                            let model_info = task.resolved_model
                                .as_ref()
                                .map(|m| format!(" using {}", m))
                                .unwrap_or_default();
                            info!("Request finished for user {}{}, duration {}",
                                user_id,
                                model_info,
                                format_duration_short(start.elapsed()));
                        } else {
                            let mut dropped = state.dropped_counts.lock().unwrap();
                            *dropped.entry(user_id.clone()).or_insert(0) += 1;
                        }
                    }
                }
                Err(e) => {
                    error!("Backend {} request failed for user {}: {}", backend_url, user_id, e);
                    let _ = task.responder.send(ResponsePart::Error(e)).await;
                    let mut dropped = state.dropped_counts.lock().unwrap();
                    *dropped.entry(user_id.clone()).or_insert(0) += 1;

                    // Log failure
                    let model_info = task.resolved_model
                        .as_ref()
                        .map(|m| format!(" using {}", m))
                        .unwrap_or_default();
                    info!("Request failed for user {}{}, duration {}",
                        user_id,
                        model_info,
                        format_duration_short(start.elapsed()));
                }
            }

            {
                let mut processing = state.processing_counts.lock().unwrap();
                if let Some(count) = processing.get_mut(&user_id) { *count = count.saturating_sub(1); }
            }
        }

        {
            let mut backends = state.backends.lock().unwrap();
            backends[backend_idx].active_requests = backends[backend_idx].active_requests.saturating_sub(1);
            backends[backend_idx].processed_count += 1;
        }
        state.backend_freed.notify_one();
    });
}

async fn handle_model_not_found(task: Task, model_name: String) {
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

fn spawn_health_checker(state: Arc<AppState>, client: reqwest::Client) {
    tokio::spawn(async move {
        // Build initial cache
        let _ = build_tags_cache(&state, &client).await;

        loop {
            let backends_to_check: Vec<(usize, String)> = {
                let backends = state.backends.lock().unwrap();
                backends.iter().enumerate().map(|(i, b)| (i, b.url.clone())).collect()
            };

            for (idx, url) in backends_to_check {
                let check_url = format!("{}/api/tags", url);
                match client.get(&check_url).send().await {
                    Ok(resp) => {
                        // Parse full model info from the response
                        let backend_models: Vec<BackendModelInfo> = resp
                            .json::<ModelsResponse>()
                            .await
                            .map(|mr| mr.models.into_iter()
                                .filter_map(|value| {
                                    serde_json::from_value::<BackendModelInfo>(value).ok()
                                })
                                .collect())
                            .unwrap_or_default();

                        let mut backends = state.backends.lock().unwrap();
                        let backend = &mut backends[idx];
                        
                        // Mark as online
                        if !backend.is_online {
                            info!("Backend {} is back online", url);
                            backend.is_online = true;
                        }
                        
                        // Update per-model status (use base-name matching)
                        let mut model_status = backend.model_status.write().unwrap();
                        let backend_model_names: HashSet<String> = backend_models.iter()
                            .map(|m| m.name.clone())
                            .collect();
                        
                        for configured_model in &backend.configured_models {
                            let config_base = configured_model.split(':').next().unwrap_or(configured_model);
                            let was_available = model_status.get(configured_model).copied().unwrap_or(true);
                            
                            // Check if backend has any model with matching base name
                            let is_available = backend_model_names.iter().any(|backend_model| {
                                let backend_base = backend_model.split(':').next().unwrap_or(backend_model);
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
                        let mut backends = state.backends.lock().unwrap();
                        let backend = &mut backends[idx];
                        
                        if backend.is_online {
                            info!("Backend {} went offline", url);
                            backend.is_online = false;
                        }
                        
                        // Mark all configured models as unavailable
                        let mut model_status = backend.model_status.write().unwrap();
                        for model_name in &backend.configured_models {
                            model_status.insert(model_name.clone(), false);
                        }
                    }
                }
            }
            
            // Build merged cache after checking all backends
            let _ = build_tags_cache(&state, &client).await;
            
            tokio::time::sleep(std::time::Duration::from_secs(state.health_check_interval)).await;
        }
    });
}

fn select_and_prepare_task(state: &Arc<AppState>, current_idx: &mut usize) -> SelectionResult {
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
        return SelectionResult::Wait;
    }

    let mut target_user = None;
    let vip_list = state.vip_user.lock().unwrap().clone();
    let boost = state.boost_user.lock().unwrap().clone();
    let mut counter = state.global_counter.lock().unwrap();

    let mut active_users: Vec<String> = queues.keys()
        .filter(|u| !queues.get(*u).unwrap().is_empty())
        .cloned()
        .collect();

    if active_users.is_empty() {
        return SelectionResult::Wait;
    }

    active_users.sort_by(|a, b| {
        let a_total = state.processed_counts.lock().unwrap().get(a).cloned().unwrap_or(0);
        let b_total = state.processed_counts.lock().unwrap().get(b).cloned().unwrap_or(0);
        a_total.cmp(&b_total).then_with(|| a.cmp(b))
    });

    // Priority: VIP users first (in order), then boost user, then round-robin
    for vip_user in &vip_list {
        if active_users.contains(vip_user) {
            target_user = Some(vip_user.clone());
            break;
        }
    }

    if target_user.is_none() {
        if let Some(ref b) = boost {
            if active_users.contains(b) && *counter % 2 == 0 { target_user = Some(b.clone()); }
        }
    }

    if target_user.is_none() {
        if *current_idx >= active_users.len() { *current_idx = 0; }
        target_user = Some(active_users[*current_idx].clone());
        *current_idx += 1;
    }

    let user_id = match target_user {
        Some(u) => u,
        None => return SelectionResult::Wait,
    };

    // Peek at the task to get the model BEFORE popping
    let task_body = queues.get(&user_id).unwrap().front().unwrap().body.clone();

    let is_debug = state.debug;
    let config = state.model_config.read().unwrap();

    // Peek at the original requested model name
    let requested_model = peek_model_from_body(&task_body);

    // Resolve alias to get the real model name (for routing decisions)
    let resolved_model = requested_model.clone()
        .and_then(|model| {
            if let Some(resolved) = config.resolve_alias(&model) {
                Some(resolved)
            } else if config.get_model(&model).is_some() {
                Some(normalize_model_tag(&model))
            } else {
                None
            }
        })
        .map(|m| normalize_model_tag(&m));

    drop(config);

    match resolved_model {
        Some(model) => {
            // Filter backends: can serve this model
            let eligible: Vec<usize> = online_indices.iter().cloned()
                .filter(|&i| backends[i].can_serve_model(&model))
                .collect();

            if eligible.is_empty() {
                // No eligible backend - wait, do NOT pop the task
                if is_debug {
                    debug!("No backend can serve model '{}' for user {}", model, user_id);
                }
                SelectionResult::Wait
            } else {
                // NOW pop the task (backend is guaranteed available)
                let mut task = queues.get_mut(&user_id).unwrap().pop_front().unwrap();
                *counter += 1;

                // Resolve alias in body (mutate the body)
                let config = state.model_config.read().unwrap();
                let resolved_model_name = extract_and_resolve_model(&mut task.body, &task.path, &config, is_debug);

                // Store resolved model in task and log if alias was used
                if let Some(ref resolved) = resolved_model_name {
                    task.resolved_model = Some(resolved.clone());

                    // Log alias rewrite if different from requested
                    let orig_normalized = requested_model
                        .as_ref()
                        .map(|m| normalize_model_tag(m))
                        .or_else(|| Some(resolved.clone()));

                    if orig_normalized.as_ref() != Some(resolved) {
                        info!("Mapped requested model {} to {} for user {}",
                            requested_model.as_deref().unwrap_or("unknown"),
                            resolved,
                            user_id);
                    }
                }

                let selected_backend_idx = {
                    let min_conns = eligible.iter()
                        .map(|&i| backends[i].active_requests)
                        .min()
                        .unwrap();
                    let candidates: Vec<usize> = eligible.iter()
                        .cloned()
                        .filter(|&i| backends[i].active_requests == min_conns)
                        .collect();
                    let candidate_pos = candidates.iter()
                        .position(|&i| i > *last_idx)
                        .unwrap_or(0);
                    let selected = candidates[candidate_pos];
                    *last_idx = selected;
                    selected
                };

                if is_debug {
                    debug!("Selected backend {} (idx {}) for user {}, model {}",
                        backends[selected_backend_idx].url,
                        selected_backend_idx,
                        user_id,
                        model
                    );
                }

                backends[selected_backend_idx].active_requests += 1;
                SelectionResult::Dispatch(
                    user_id,
                    task,
                    selected_backend_idx,
                    backends[selected_backend_idx].url.clone()
                )
            }
        }
        None => {
            // Model not in config - pop and fail
            let task = queues.get_mut(&user_id).unwrap().pop_front().unwrap();
            *counter += 1;

            if is_debug {
                debug!("Model not in config for user {}", user_id);
            }

            SelectionResult::ModelNotFound(task, requested_model.unwrap_or("unknown".to_string()))
        }
    }
}

pub async fn run_worker(state: Arc<AppState>) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(state.timeout))
        .build()
        .unwrap();
    let mut current_idx = 0;

    spawn_health_checker(state.clone(), client.clone());

    loop {
        let selection = select_and_prepare_task(&state, &mut current_idx);

        match selection {
            SelectionResult::ModelNotFound(task, model_name) => {
                handle_model_not_found(task, model_name).await;
            }
            SelectionResult::Dispatch(user_id, task, backend_idx, backend_url) => {
                dispatch_task(DispatchTaskArgs {
                    user_id,
                    task,
                    backend_idx,
                    backend_url,
                    state: state.clone(),
                    client: client.clone(),
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
    let ip = extract_client_ip(&headers, addr, &state.ip_header);
    let is_debug = state.debug;

    // --- Authentication ---
    let user_id = match authenticate_request(&headers, &state.user_registry, ip, is_debug) {
        Some(uid) => uid,
        None => return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
    };

    if is_debug {
        debug!("Request from user: {} to {} {}", user_id, method, path);
        if path.starts_with("/api/generate") || path.starts_with("/api/chat") {
            if let Ok(body_str) = std::str::from_utf8(&body) {
                debug!("Request body: {}", body_str);
            }
        }
    }

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
    task_headers.remove(axum::http::header::AUTHORIZATION);
    task_headers.remove(axum::http::header::CONTENT_LENGTH);

    let normalized_path = normalize_path(&path);

    let task = Task {
        path: normalized_path.to_string(),
        method,
        headers: task_headers,
        responder: tx,
        body,
        resolved_model: None,
    };

    {
        let mut queues = state.queues.lock().unwrap();
        queues
            .entry(user_id.clone())
            .or_insert_with(VecDeque::new)
            .push_back(task);
    }

    state.notify.notify_one();

    if is_debug {
        debug!("Task queued for user: {}", user_id);
    }

    let mut rx = rx;
    match rx.recv().await {
        Some(ResponsePart::Status(status, headers)) => {
            if is_debug {
                debug!("Received response status {} for user: {}", status, user_id);
            }
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
            error!("Backend error for user {}: {}", user_id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Backend error: {}", e)).into_response()
        }
        _ => {
            error!("Worker failed to respond for user {}", user_id);
            (StatusCode::INTERNAL_SERVER_ERROR, "Worker failed to respond").into_response()
        }
    }
}

pub async fn tags_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    _method: Method,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Extract IP
    let ip = extract_client_ip(&headers, addr, &state.ip_header);

    // Authentication
    let user_id = match authenticate_request(&headers, &state.user_registry, ip, true) {
        Some(uid) => uid,
        None => return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
    };

    // Check blocking
    if state.is_ip_blocked(&ip) {
        warn!("Blocked tags request from IP: {} for user: {}", ip, user_id);
        return (StatusCode::FORBIDDEN, "IP blocked").into_response();
    }
    if state.is_user_blocked(&user_id) {
        warn!("Blocked tags request from user: {} (IP: {})", user_id, ip);
        return (StatusCode::FORBIDDEN, "User blocked").into_response();
    }

    // Return cached response (or empty list if not populated)
    let cache = state.cached_tags.read().unwrap();
    match cache.as_ref() {
        Some(cached_tags) => (StatusCode::OK, axum::Json(cached_tags.clone())).into_response(),
        None => (StatusCode::OK, axum::Json(serde_json::json!({"models": []}))).into_response(),
    }
}

/// OpenAI-compatible models list handler
pub async fn models_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let models_list = build_models_list(&state.cached_tags.read().unwrap());
    (StatusCode::OK, axum::Json(models_list)).into_response()
}

/// OpenAI-compatible single model handler
pub async fn model_handler(
    State(state): State<Arc<AppState>>,
    Path(model_name): Path<String>,
) -> impl IntoResponse {
    match find_model_by_name(&state, &model_name) {
        Some(model) => (StatusCode::OK, axum::Json(model)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "model_not_found",
                    "message": format!("Model '{}' not found", model_name)
                }
            }))
        ).into_response(),
    }
}

#[derive(Serialize)]
struct HealthResponse {
    models: HashMap<String, String>,
}

pub async fn health_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    // Simple authentication - no IP tracking, minimal logging
    let valid = match headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        Some(token) => {
            state.user_registry.lock().unwrap().authenticate(token).is_some()
        }
        None => false,
    };

    if !valid {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let mut model_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();
    
    let backends = state.backends.lock().unwrap();
    
    for backend in backends.iter() {
        let model_status = backend.model_status.read().unwrap();
        
        for (model_name, &available) in model_status.iter() {
            let counts = model_counts.entry(model_name.clone()).or_insert_with(HashMap::new);
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
        
        models.insert(model_name.clone(), status.to_string());
    }
    
    (StatusCode::OK, axum::Json(HealthResponse { models })).into_response()
}
