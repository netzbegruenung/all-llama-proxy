use axum::{
    Router,
    routing::{any, get},
};
use clap::Parser;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Notify;
use tracing::{info, Level, warn};
use tracing_subscriber::EnvFilter;
use std::fmt;

mod auth;
mod dispatcher;
mod tui;

use crate::auth::UserRegistry;
use crate::dispatcher::{AppState, LogBuffer, LogBufferWriter, proxy_handler, tags_handler, run_worker, models_handler, model_handler, health_handler};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Address to bind to (e.g., 127.0.0.1:11435, 0.0.0.0:8080, or [::1]:11435)
    #[arg(short, long, default_value = "127.0.0.1:11435")]
    bind: String,

    /// Request timeout in seconds
    #[arg(short, long, default_value_t = 300)]
    timeout: u64,

    /// Enable TUI dashboard
    #[arg(long)]
    tui: bool,

    /// Path to users.yaml file for authentication
    #[arg(long, default_value = "/etc/all-llama-proxy/users.yaml")]
    users_path: String,

    /// Path to models.yaml file (required - backends come from here)
    #[arg(long, default_value = "/etc/all-llama-proxy/models.yaml")]
    model_config_path: String,

    /// Enable verbose debug logging
    #[arg(long)]
    debug: bool,

    /// HTTP header to extract real client IP from (e.g., X-Real-IP, X-Forwarded-For)
    #[arg(short = 'i', long = "ip-header")]
    ip_header: Option<String>,
}

struct TuiState {
    visible: bool,
    toggle_notify: Arc<Notify>,
}

struct SimpleFormatter;

impl<S, N> tracing_subscriber::fmt::format::FormatEvent<S, N> for SimpleFormatter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::format::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        let level = event.metadata().level();
        let level_str = match *level {
            Level::ERROR => "ERROR",
            Level::WARN => "WARN",
            Level::INFO => "INFO",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "TRACE",
        };
        
        write!(writer, "{}: ", level_str)?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Determine if we should run TUI
    let use_tui = args.tui && std::io::stdout().is_terminal();

    // Configure logging based on TUI mode
    let log_level = if args.debug { "debug" } else { "info" };

    let log_buffer = if use_tui {
        // TUI mode: create shared log buffer, log to stderr (won't corrupt TUI stdout)
        let log_buffer = LogBuffer::new(100);
        
        tracing_subscriber::fmt()
            .with_writer(LogBufferWriter::new(log_buffer.clone()))
            .with_ansi(false)
            .event_format(SimpleFormatter)
            .with_env_filter(EnvFilter::new(log_level))
            .init();
        
        Some(log_buffer)
    } else {
        // Non-TUI mode: log to stdout with flat format
        tracing_subscriber::fmt()
            .with_writer(std::io::stdout)
            .with_ansi(false)
            .event_format(SimpleFormatter)
            .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level)))
            .init();
        
        None
    };

    // Load user registry from CLI-specified path.
    // Load will fail if not found, but we'll fall back to empty
    let registry = match UserRegistry::load(&args.users_path) {
        Ok(r) => {
            info!("Loaded user registry from {}", args.users_path);
            r
        }
        Err(e) => {
            warn!("Could not load {}: {}. All requests will be rejected.", args.users_path, e);
            UserRegistry::empty()
        }
    };

    // Load model config (required, will crash if missing)
    let log_buffer = log_buffer.unwrap_or_else(|| LogBuffer::new(100));
    
    let state = Arc::new(AppState::new(
        args.model_config_path.clone(),
        args.timeout,
        registry,
        args.debug,
        log_buffer.clone(),
        args.ip_header,
    ).expect("Failed to load model configuration"));

    // Reload the user registry and model config on SIGHUP without restarting.
    let sighup_state = state.clone();
    let users_path = args.users_path.clone();
    let config_path = args.model_config_path.clone();
    tokio::spawn(async move {
        let mut sig = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                warn!("Could not register SIGHUP handler: {}", e);
                return;
            }
        };
        loop {
            sig.recv().await;
            match UserRegistry::load(&users_path) {
                Ok(new_registry) => {
                    let new_registry_arc = Arc::new(new_registry.clone());
                    *sighup_state.user_registry.lock().unwrap() = new_registry_arc.clone();
                    *sighup_state.vip_user.lock().unwrap() = new_registry_arc.get_vip_users();
                    info!("User registry reloaded from {} via SIGHUP", users_path);
                }
                Err(e) => {
                    warn!("Failed to reload {} via SIGHUP: {}", users_path, e);
                }
            }
            if let Err(e) = sighup_state.reload_model_config(&config_path) {
                eprintln!("ERROR: Failed to reload model config from {}: {}", config_path, e);
                eprintln!("Continuing with existing configuration");
            } else {
                info!("Model config reloaded from {} via SIGHUP", config_path);
            }
        }
    });

    let worker_state = state.clone();
    tokio::spawn(async move {
        run_worker(worker_state).await;
    });

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/", any(proxy_handler))
        .route("/api/generate", any(proxy_handler))
        .route("/api/chat", any(proxy_handler))
        .route("/api/embed", any(proxy_handler))
        .route("/api/embeddings", any(proxy_handler))
        .route("/api/tags", get(tags_handler))
        .route("/api/show", any(proxy_handler))
        .route("/api/create", any(proxy_handler))
        .route("/api/copy", any(proxy_handler))
        .route("/api/delete", any(proxy_handler))
        .route("/api/pull", any(proxy_handler))
        .route("/api/push", any(proxy_handler))
        .route("/api/blobs/{digest}", any(proxy_handler))
        .route("/api/ps", any(proxy_handler))
        .route("/api/version", any(proxy_handler))
        .route("/chat/completions", any(proxy_handler))
        .route("/v1/chat/completions", any(proxy_handler))
        .route("/v1/completions", any(proxy_handler))
        .route("/v1/embeddings", any(proxy_handler))
        .route("/v1/responses", any(proxy_handler))
        .route("/v1/images/generations", any(proxy_handler))
        .route("/models", get(models_handler))
        .route("/v1/models", get(models_handler))
        .route("/models/{model_name}", get(model_handler))
        .route("/v1/models/{model_name}", get(model_handler))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024 * 1024)) // 1GB limit
        .with_state(state.clone());

    let addr = format!("0.0.0.0:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("Dispatcher running on http://{}", addr);

    if use_tui {
        let tui_state = Arc::new(Mutex::new(TuiState {
            visible: true,
            toggle_notify: Arc::new(Notify::new()),
        }));

        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        // Run TUI on the main thread
        tui_loop(tui_state, state).await;
    } else {
        // Just run the server on the main thread
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    }
}

async fn tui_loop(tui_state: Arc<Mutex<TuiState>>, state: Arc<AppState>) {
    let mut dashboard = tui::TuiDashboard::new();
    let toggle_notify = Arc::new(tui_state.lock().unwrap().toggle_notify.clone());

    loop {
        let visible = {
            let tui_state = tui_state.lock().unwrap();
            tui_state.visible
        };

        if visible {
            match dashboard.run(&state) {
                Ok(continue_loop) => {
                    if !continue_loop {
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("TUI error: {}", e);
                    return;
                }
            }
        } else {
            toggle_notify.notified().await;
        }
    }
}
