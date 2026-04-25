use crate::dispatcher::AppState;
use crate::protocol::{BackendSnapshot, DashboardCmd, DashboardSnapshot, consumed_len, decode};
use crate::utils::LockExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Duration, interval};
use tracing::{info, warn};

const CHUNK_SIZE: usize = 8192;

pub struct DashboardServer {
    serve_mode: ServeMode,
}

pub enum ServeMode {
    File(PathBuf),
    Activated(tokio::net::UnixListener),
}

impl DashboardServer {
    pub fn new(socket_path: PathBuf) -> std::io::Result<Self> {
        Ok(Self {
            serve_mode: ServeMode::File(socket_path),
        })
    }

    pub fn from_systemd() -> std::io::Result<Option<Self>> {
        let mut listenfd = listenfd::ListenFd::from_env();
        if let Some(unix_listener) = listenfd.take_unix_listener(0)? {
            let tokio_listener = tokio::net::UnixListener::from_std(unix_listener)?;
            info!("Dashboard activated via systemd socket");
            return Ok(Some(Self {
                serve_mode: ServeMode::Activated(tokio_listener),
            }));
        }
        Ok(None)
    }

    pub async fn serve(self, state: Arc<AppState>) -> std::io::Result<()> {
        let listener = match self.serve_mode {
            ServeMode::File(socket_path) => {
                let _ = std::fs::remove_file(&socket_path);
                let listener = tokio::net::UnixListener::bind(&socket_path)?;
                info!("Dashboard listening on {}", socket_path.display());

                let socket_path = socket_path.clone();
                tokio::spawn(async move {
                    tokio::signal::ctrl_c().await.ok();
                    let _ = std::fs::remove_file(&socket_path);
                });

                listener
            }
            ServeMode::Activated(listener) => {
                info!("Dashboard listening on systemd-activated socket");
                listener
            }
        };

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    info!("Dashboard client connected to {:?}", addr);
                    let sub_state = state.clone();

                    let (push_tx, mut push_rx) = tokio::sync::mpsc::channel::<DashboardSnapshot>(8);
                    let (wire_reader, mut wire_writer) = stream.into_split();

                    let push_state = sub_state.clone();
                    tokio::spawn(async move {
                        Self::push_loop(push_state, push_tx).await;
                    });

                    tokio::spawn(async move {
                        while let Some(snap) = push_rx.recv().await {
                            if let Ok(bytes) = crate::protocol::encode(&snap) {
                                if wire_writer.write_all(&bytes).await.is_err() {
                                    break;
                                }
                                if wire_writer.flush().await.is_err() {
                                    break;
                                }
                            }
                        }
                    });

                    tokio::spawn(async move {
                        let _ = Self::handle_clients(wire_reader, sub_state).await;
                    });
                }
                Err(e) => {
                    warn!("Dashboard accept error: {}", e);
                }
            }
        }
    }

    async fn push_loop(state: Arc<AppState>, tx: tokio::sync::mpsc::Sender<DashboardSnapshot>) {
        let mut itv = interval(Duration::from_millis(100));
        itv.tick().await;
        loop {
            itv.tick().await;
            if let Ok(snapshot) = Self::capture_snapshot(&state) {
                let _ = tx.send(snapshot).await;
            }
        }
    }

    async fn handle_clients(
        mut reader: tokio::net::unix::OwnedReadHalf,
        state: Arc<AppState>,
    ) -> std::io::Result<()> {
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut read_buf = Vec::new();
        loop {
            match reader.read(&mut buf[..]).await {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    read_buf.extend_from_slice(&buf[..n]);
                    while read_buf.len() >= 4 {
                        if let Some(consumed) = consumed_len(&read_buf) {
                            let payload = read_buf.drain(..consumed).collect::<Vec<_>>();
                            if let Ok(Some(cmd)) = decode::<DashboardCmd>(&payload) {
                                Self::apply_cmd(&state, cmd);
                            }
                        } else {
                            break;
                        }
                    }
                }
                Err(_) => return Ok(()),
            }
        }
    }

    fn capture_snapshot(
        state: &AppState,
    ) -> Result<DashboardSnapshot, Box<dyn std::error::Error + Send + Sync>> {
        let queues_len: HashMap<String, usize> = {
            let q = state.queues.lock().lock_unwrap("queues");
            q.iter().map(|(k, v)| (k.clone(), v.len())).collect()
        };
        let processing_counts = state
            .processing_counts
            .lock()
            .lock_unwrap("processing_counts")
            .clone();
        let processed_counts = state
            .processed_counts
            .lock()
            .lock_unwrap("processed_counts")
            .clone();
        let dropped_counts = state
            .dropped_counts
            .lock()
            .lock_unwrap("dropped_counts")
            .clone();
        let user_ips = state.user_ips.lock().lock_unwrap("user_ips").clone();
        let blocked_ips = state.blocked_ips.lock().lock_unwrap("blocked_ips").clone();
        let blocked_users = state
            .blocked_users
            .lock()
            .lock_unwrap("blocked_users")
            .clone();
        let vip_list = state.vip_user.lock().lock_unwrap("vip_user").clone();
        let backends = state.backends.lock().lock_unwrap("backends");

        let model_public_names: HashMap<String, String> = {
            let config = state.model_config.read().expect("model_config read");
            config
                .models
                .iter()
                .map(|m| {
                    let display = m.public_name.clone().unwrap_or_else(|| m.name.clone());
                    (m.name.clone(), display)
                })
                .collect()
        };

        let log_lines = state.log_buffer.get_last_n(10);

        let mut user_ids: Vec<String> = queues_len.keys().cloned().collect();
        user_ids.sort_by(|a, b| {
            let a_q = queues_len.get(a).unwrap_or(&0) + processing_counts.get(a).unwrap_or(&0);
            let b_q = queues_len.get(b).unwrap_or(&0) + processing_counts.get(b).unwrap_or(&0);
            let a_total =
                processed_counts.get(a).unwrap_or(&0) + dropped_counts.get(a).unwrap_or(&0);
            let b_total =
                processed_counts.get(b).unwrap_or(&0) + dropped_counts.get(b).unwrap_or(&0);
            b_q.cmp(&a_q)
                .then_with(|| b_total.cmp(&a_total))
                .then_with(|| a.cmp(b))
        });

        let backends_wire: Vec<BackendSnapshot> = backends
            .iter()
            .map(|b| {
                let model_status = b.model_status.read().expect("model_status read");
                let ms = model_status.clone();
                BackendSnapshot {
                    url: b.url.clone(),
                    active_requests: b.active_requests,
                    processed_count: b.processed_count,
                    is_online: b.is_online,
                    active_models: b.active_models.clone(),
                    processed_models: b.processed_models.clone(),
                    configured_models: b.configured_models.clone(),
                    model_status: ms,
                }
            })
            .collect();

        Ok(DashboardSnapshot {
            queues_len,
            processing_counts,
            processed_counts,
            dropped_counts,
            user_ips: user_ips
                .iter()
                .map(|(k, v)| (k.clone(), v.to_string()))
                .collect(),
            blocked_ips: blocked_ips.iter().map(|ip| ip.to_string()).collect(),
            blocked_users: blocked_users.iter().cloned().collect(),
            vip_list,
            user_ids,
            backends: backends_wire,
            model_public_names,
            log_lines,
        })
    }

    fn apply_cmd(state: &AppState, cmd: DashboardCmd) {
        match cmd {
            DashboardCmd::ToggleVip(user_id) => {
                let mut vip_list = state.vip_user.lock().lock_unwrap("vip_user");
                if vip_list.contains(&user_id) {
                    vip_list.retain(|u| u != &user_id);
                    info!("VIP removed: {}", user_id);
                } else {
                    let uid_clone = user_id.clone();
                    vip_list.push(uid_clone);
                    info!("VIP granted: {}", user_id);
                }
            }
            DashboardCmd::BlockUser(user_id) => {
                let mut blocked_users = state.blocked_users.lock().lock_unwrap("blocked_users");
                if blocked_users.insert(user_id.clone()) {
                    info!("User blocked: {}", user_id);
                }
            }
            DashboardCmd::UnblockUser(user_id) => {
                let mut blocked_users = state.blocked_users.lock().lock_unwrap("blocked_users");
                if blocked_users.remove(&user_id) {
                    info!("User unblocked: {}", user_id);
                }
            }
            DashboardCmd::BlockIp(ip_str) => {
                if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
                    let mut blocked_ips = state.blocked_ips.lock().lock_unwrap("blocked_ips");
                    if blocked_ips.insert(ip) {
                        info!("IP blocked: {}", ip_str);
                    }
                } else {
                    warn!("Invalid IP address to block: {}", ip_str);
                }
            }
            DashboardCmd::UnblockIp(ip_str) => {
                if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
                    let mut blocked_ips = state.blocked_ips.lock().lock_unwrap("blocked_ips");
                    if blocked_ips.remove(&ip) {
                        info!("IP unblocked: {}", ip_str);
                    }
                } else {
                    warn!("Invalid IP address to unblock: {}", ip_str);
                }
            }
        }
    }
}
