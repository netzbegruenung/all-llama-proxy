# AGENTS.md

## Project Overview

**all-llama-proxy** — Rust binary (2 binaries + 1 lib) that queues and load-balances requests across multiple Ollama backends with per-user fair-share scheduling and a real-time TUI dashboard.

- **Binaries:** `all-llama-proxy` (main.rs:1) — HTTP proxy server; `all-llama-tui` (tui.rs:321) — interactive dashboard
- **Lib:** `all_llama_proxy` (lib.rs:1) — re-exports: `AppState`, `DashboardServer`, `UserRegistry`, `LogBuffer`, handler functions, and protocol types
- **Two configs:** `/etc/all-llama-proxy/models.yaml` (model→backend mappings + aliases) and `users.yaml` (SHA256 token hashes)
- **Default bind:** `127.0.0.1:11435`; dashboard socket: `/run/all-llama-proxy.sock`

## Commands

```bash
cargo build
cargo build --release
cargo fmt --all -- --check                                                              # check formatting (CI requirement)
cargo run -- --bind 0.0.0.0:8080 --models-config-path ./examples/models.yaml --users-path ./examples/users.yaml     # dev run
cargo run --bin all-llama-tui                                                         # launch TUI dashboard
cargo test                                                                            # auth.rs unit tests (line 101-189)
./test_dispatcher.sh                                                                  # stress test; seeds ./examples/users-test.yaml
```

`cargo install --path .` for release builds.

## Architecture (how it works)

```
Client → axum routes (main.rs:153) → proxy_handler (dispatcher.rs:1349)
  → auth (auth.rs:55): Bearer token SHA256 match → per-user FIFO queue
  → run_worker (dispatcher.rs:1313): fair-share scheduler picks user → finds compatible backend
    (least-connections round-robin, VIP-first) → dispatch_task (dispatcher.rs:915)
    → reqwest → backend → stream ResponsePart back to client
```

**Key files:**
- `src/main.rs` — CLI args, routes, SIGHUP reload (line 118), dashboard init
- `src/dispatcher.rs` — AppState, worker loop, proxy handler, health checker (line 1054), alias resolution
- `src/auth.rs` — UserRegistry: SHA256 token matching with constant-time comparison
- `src/tui.rs` — standalone binary, reads snapshot via Unix socket
- `src/dashboard_server.rs` — bincode-encoded snapshots pushed every 100ms, accepts DashboardCmd
- `src/protocol.rs` — `encode`/`decode` wire format: 4-byte BE length prefix + bincode payload

**SIGHUP (main.rs:118):** reloads both `users.yaml` and `models.yaml` at runtime, then triggers immediate keep-alive for all models with `keep_alive: true`.

**Blocked items** persist to `blocked_items.json` at the process working directory (dispatcher.rs:23).

**Model keep-alive:** Runs every 15 minutes, on startup, and after SIGHUP reloads. Models can disable keep-alive by setting `keep_alive: false` in models.yaml (useful for embedding models). (dispatcher.rs:1341)

The warm-up request is a 1-token chat completion against `POST {backend}/v1/chat/completions` (health.rs:395), which both Ollama and llama.cpp serve — unlike `/api/generate`, which llama.cpp answers with 404. The proxy no longer sends a `keep_alive` parameter, so **indefinite residency must be configured on the backend**: set `OLLAMA_KEEP_ALIVE=-1` on Ollama hosts, otherwise models unload after the default 5-minute TTL between keeper cycles. llama.cpp keeps models resident inherently.

**Backend model discovery** (`fetch_backend_models`, health.rs:52) probes `/api/tags` first, then falls back to `/v1/models`. Ollama serves both; **llama.cpp serves only `/v1/models`** and answers `/api/tags` with a 404 JSON error body. The endpoint that worked is remembered per backend for the lifetime of the health-check task, so the steady state costs one request.

A response is accepted only if the status is 2xx **and** the body parses. The status check is load-bearing: both list fields (`models`, `data`) are `#[serde(default)]`, so a 404 error body would otherwise deserialize into an empty list and be indistinguishable from a backend with no models. If neither endpoint yields a list, the backend is marked **offline** (health.rs:354) — distinct from `Unreachable`, but treated the same way, since `can_serve_model` (appstate.rs:299) gates routing at dispatcher.rs:551.

Entry parsing is deliberately tolerant (health.rs:149): llama.cpp sends `"size": ""` where Ollama sends a number, and omits other fields entirely. `size` accepts number/string/empty, all other fields default, and the name resolves `name` → `model` → `id` (the last covers plain OpenAI `data[]` lists). Unparseable or unnamed entries are logged at `debug!` rather than silently dropped.

## Config format

**models.yaml:**
```yaml
models:
  - name: "qwen3:35b"
    public_name: "qwen35"
    backends: [http://host1:11434, http://host2:11434]
    aliases: [qwen3, qwen3:35b]
    keep_alive: true  # default: true; set to false for embedding models (dispatcher.rs:337)
```

**users.yaml:**
```yaml
users:
  - token_hash: abc123...
    user_id: alice
    vip: true
```

Generate token hash: `echo -n "token" | sha256sum`

## Deployment

Example systemd files in `examples/all-llama-proxy.service` and `examples/all-llama-proxy.socket`.

## Framework/toolchain notes

- **Edition 2024** (Cargo.toml:4)
- Built on `tokio` + `axum` + `ratatui` + `crossterm`
- Health checker queries `/api/tags` every 10s per backend (dispatcher.rs:1054)
- Model aliases are resolved in-body before dispatch (dispatcher.rs:696) — the `model` JSON field is rewritten to the real name
- `normalize_model_tag` (dispatcher.rs:751) appends `:latest` if no tag present
- Max body limit: 1 GB (main.rs:180)
- Auth uses `subtle::ConstantTimeEq` — never break the scanning loop (auth.rs:72)
- Dashboard supports both standalone Unix socket and systemd socket activation (dashboard_server.rs:28)
