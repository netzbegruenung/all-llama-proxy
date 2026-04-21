use all_llama_proxy::{
    DashboardServer, UserRegistry, AppState, LogBuffer, LogBufferWriter,
    proxy_handler, tags_handler, models_handler, model_handler, health_handler, run_worker,
};
use axum::{
    Router,
    routing::{any, get},
};
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use std::fmt;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:11435")]
    bind: String,

    #[arg(short, long, default_value_t = 300)]
    timeout: u64,

    #[arg(long, default_value = "/run/all-llama-proxy.sock")]
    dashboard_socket: String,

    #[arg(long, default_value = "/etc/all-llama-proxy/users.yaml")]
    users_path: String,

    #[arg(long, default_value = "/etc/all-llama-proxy/models.yaml")]
    model_config_path: String,

    #[arg(long)]
    debug: bool,

    #[arg(short = 'i', long = "ip-header")]
    ip_header: Option<String>,
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
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARN",
            tracing::Level::INFO => "INFO",
            tracing::Level::DEBUG => "DEBUG",
            tracing::Level::TRACE => "TRACE",
        };

        write!(writer, "{}: ", level_str)?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let addr = args.bind.parse::<SocketAddr>()
        .expect("Invalid bind address. Use format like 127.0.0.1:11435, 0.0.0.0:8080, or [::1]:11435");

    let log_level = if args.debug { "debug" } else { "info" };

    let log_buffer = {
        let log_buffer = LogBuffer::new(100);

        tracing_subscriber::fmt()
            .with_writer(LogBufferWriter::new(log_buffer.clone()))
            .with_ansi(false)
            .event_format(SimpleFormatter)
            .with_env_filter(EnvFilter::new(log_level))
            .init();

        log_buffer
    };

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

    let log_buffer_clone = log_buffer.clone();
    let state = Arc::new(AppState::new(
        args.model_config_path.clone(),
        args.timeout,
        registry,
        args.debug,
        log_buffer_clone,
        args.ip_header,
        10,
    ).expect("Failed to load model configuration"));

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
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!("Dispatcher running on http://{}", addr);

    if let Ok(Some(dashboard_server)) = DashboardServer::from_systemd() {
        let dashboard_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = dashboard_server.serve(dashboard_state).await {
                eprintln!("Dashboard server error: {}", e);
            }
        });
        info!("Dashboard activated via systemd socket");
    } else {
        let socket_path_buf = PathBuf::from(&args.dashboard_socket);
        let dashboard_state = state.clone();
        tokio::spawn(async move {
            let server = match DashboardServer::new(socket_path_buf) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Dashboard server error: {}", e);
                    return;
                }
            };
            if let Err(e) = server.serve(dashboard_state).await {
                eprintln!("Dashboard server error: {}", e);
            }
        });
        info!("Dashboard enabled at {}", args.dashboard_socket);
    }

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}
