use bincode::config::standard;
use bincode::serde::decode_from_slice;
use bincode::serde::encode_to_vec;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ───────────────
// Server → TUI types
// ───────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub queues_len: HashMap<String, usize>,
    pub processing_counts: HashMap<String, usize>,
    pub processed_counts: HashMap<String, usize>,
    pub dropped_counts: HashMap<String, usize>,
    pub user_ips: HashMap<String, String>,
    pub blocked_ips: HashSet<String>,
    pub blocked_users: HashSet<String>,
    pub vip_list: Vec<String>,
    pub user_ids: Vec<String>,
    pub backends: Vec<BackendSnapshot>,
    pub model_public_names: HashMap<String, String>,
    pub log_lines: Vec<(String, i64, String)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackendSnapshot {
    pub url: String,
    pub active_requests: usize,
    pub processed_count: usize,
    pub is_online: bool,
    pub active_models: HashMap<String, usize>,
    pub processed_models: HashMap<String, usize>,
    pub configured_models: Vec<String>,
    pub model_status: HashMap<String, bool>,
}

// ───────────────
// TUI → Server types
// ───────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DashboardCmd {
    ToggleVip(String),
    BlockUser(String),
    UnblockUser(String),
    BlockIp(String),
    UnblockIp(String),
}

// ───────────────
// Wire helpers
// ───────────────

pub fn encode<T: Serialize + std::fmt::Debug>(payload: &T) -> std::io::Result<Vec<u8>> {
    let config = standard();
    let body = encode_to_vec(payload, config).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bincode encode error: {:?}, payload: {:?}", e, payload),
        )
    })?;

    let len = (body.len() as u32).to_be_bytes();
    let mut msg = Vec::with_capacity(body.len() + 4);
    msg.extend_from_slice(&len);
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// Returns the message body length from the 4-byte big-endian prefix, or None if not enough bytes.
fn prefix_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    let len_bytes: [u8; 4] = [buf[0], buf[1], buf[2], buf[3]];
    Some(u32::from_be_bytes(len_bytes) as usize)
}

pub fn decode<T: for<'a> Deserialize<'a>>(buf: &[u8]) -> std::io::Result<Option<T>> {
    let len = match prefix_len(buf) {
        Some(l) => l,
        None => return Ok(None),
    };
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let data = &buf[4..4 + len];
    let config = standard();
    decode_from_slice::<T, _>(data, config)
        .map(|(value, _)| Some(value))
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bincode decode error: {}", e),
            )
        })
}

/// Returns how many bytes to consume after successfully decoding, or None if not a valid message.
pub fn consumed_len(buf: &[u8]) -> Option<usize> {
    let len = prefix_len(buf)?;
    if buf.len() >= 4 + len {
        Some(4 + len)
    } else {
        None
    }
}
