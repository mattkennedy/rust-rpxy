# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`rpxy` is a high-performance HTTP reverse proxy written in Rust, supporting HTTP/1.1, HTTP/2, and HTTP/3 with TLS termination, ACME certificate automation, and advanced routing features. It significantly outperforms NGINX in benchmarks (30-60% faster).

## Development Commands

### Building

```bash
# Standard build (HTTP/3 with quinn, default)
cargo build --release

# Build with s2n-quic instead of quinn (requires OpenSSL for building)
cargo build --no-default-features --features http3-s2n --release

# Development build
cargo build
```

### Testing

```bash
# Run all tests
cargo test --verbose

# Run tests for a specific package
cargo test -p rpxy-lib
cargo test -p rpxy-certs
cargo test -p rpxy-acme
cargo test -p rpxy-bin
```

### Running

```bash
# Run with config file
cargo run --release -- --config config.toml

# Run with logging to directory
cargo run --release -- --config config.toml --log-dir ./logs
```

### Code Quality

```bash
# Format code (required before PR)
cargo fmt

# Check formatting
cargo fmt -- --check

# Run clippy
cargo clippy
```

### Submodules

```bash
# Initialize submodules after cloning
git submodule update --init
```

## Workspace Architecture

This is a Cargo workspace with four members that interact hierarchically:

### `rpxy-bin` (Binary Entry Point)
- CLI argument parsing and orchestration
- Configuration hot-reloading via `hot_reload` crate
- Service lifecycle management
- Wires together all components (proxy config, app config, certificates, ACME)
- **Key files**: `rpxy-bin/src/main.rs`, `rpxy-bin/src/log.rs`

### `rpxy-lib` (Core Proxy Logic)
- HTTP/1.1, HTTP/2, HTTP/3 protocol handling
- Request routing and message manipulation
- Load balancing (round-robin, random, sticky sessions)
- **Automatic upstream failover** (new)
- TLS termination
- Response caching (optional feature)
- **Key modules**:
  - `message_handler/`: Request routing and processing
  - `forwarder/`: Upstream forwarding logic
  - `hyper_executor/`: Custom hyper executor
  - `backend/`: Load balancing, upstream selection, and failover
  - `crypto/`: TLS configuration management
  - `h3_/`: HTTP/3 implementation (quinn or s2n-quic)

### `rpxy-certs` (Certificate Management)
- TLS certificate loading from disk
- Hot-reloading via file watching
- Per-SNI ServerConfig construction for HTTP/1.1 and HTTP/2
- Aggregated ServerConfig for HTTP/3
- Post-quantum cryptography support (via `rustls-post-quantum`)
- **Key files**: `rpxy-certs/src/lib.rs`, `rpxy-certs/src/reloader.rs`

### `rpxy-acme` (ACME Protocol)
- Automatic certificate issuance and renewal
- TLS-ALPN-01 challenge handling
- Certificate caching and lifecycle management
- **Only active when `acme` feature is enabled**
- **Key files**: `rpxy-acme/src/lib.rs`

## Request Flow Architecture

```
Client → Socket (TCP/UDP) → Protocol Handler Selection
  ├─ HTTP cleartext → Message Handler
  ├─ HTTPS → Lazy TLS Handshake → SNI Extraction → ServerConfig Lookup → Message Handler
  └─ HTTP/3 → QUIC Connection → Message Handler

Message Handler:
  1. Extract Host header / SNI
  2. Find backend app by server_name
  3. Validate SNI↔Host consistency (if enabled)
  4. Apply HTTP→HTTPS redirect (if configured)
  5. Path-based routing (longest prefix match)
  6. Load balancer upstream selection
  7. Request header manipulation (Forwarded, X-Forwarded-*, Host rewrite)

Forwarder:
  1. Check response cache (if cache feature enabled)
  2. Forward to upstream with appropriate TLS/ALPN
  3. Handle connection upgrades (WebSocket, H2C)
  4. Cache response (if applicable)

Response Processing:
  1. Remove hop-by-hop headers
  2. Add Alt-SVC header (for HTTP/3 advertisement)
  3. Apply sticky cookie (if enabled)
  4. Return to client
```

## Configuration Hot-Reloading

The configuration file (`config.toml`) is watched for changes in real-time:

1. File watcher detects modification
2. Parse and validate new configuration
3. Spawn new proxy services with new config
4. Cancel old services via cancellation token
5. Gracefully drain existing connections
6. Rebind sockets using `SO_REUSEADDR`/`SO_REUSEPORT`

**Important**: Removing/renaming `config.toml` keeps the proxy running with the last valid config until a new file appears.

## TLS and Certificate Management

### Certificate Loading Flow
1. Configuration specifies per-app cert paths: `tls_cert_path`, `tls_cert_key_path`
2. `CryptoFileSource` reads PEM files from disk
3. `CryptoReloader` polls for file changes (default: 60s interval)
4. On change, rebuild ServerConfigs without stopping the proxy
5. Create two variants:
   - **Per-SNI map**: Individual ServerConfigs for TCP/TLS (supports client auth)
   - **Aggregated config**: Single ServerConfig for QUIC/HTTP/3 (no client auth support yet)

### TLS Handshake (TCP/HTTPS)
- Uses `tokio_rustls::LazyConfigAcceptor` for SNI extraction before full handshake
- Lookup ServerConfig by SNI from map
- Complete async handshake with timeout
- **Critical feature**: SNI↔Host header consistency validation prevents domain fronting

### ACME Integration
- Enabled per-app with `tls = { acme = true }`
- Requires global `[experimental.acme]` settings
- Spawns separate task per domain for renewal
- Handles TLS-ALPN-01 challenge validation
- Certificates cached locally to avoid re-issuance

**Note**: Private keys must be in PKCS8 format (not PKCS1). See README TIPS section for conversion.

## HTTP Protocol Handling

### HTTP/1.1 & HTTP/2 (TCP)
- Built on `hyper` with custom `LocalExecutor`
- Single shared `ConnectionBuilder` for all TCP listeners
- Supports connection upgrades (WebSocket, H2C via `serve_connection_with_upgrades`)
- Separate HTTP/2-only client for upstream H2C
- Configurable keep-alive and idle timeouts

### HTTP/3 (QUIC) - Experimental
Two mutually exclusive implementations:
- **`http3-quinn`** (default): Uses `quinn` + `h3` + `h3-quinn`
- **`http3-s2n`**: Uses AWS `s2n-quic` + `s2n-quic-h3`

**Key differences**:
- `s2n-quic` requires OpenSSL at build time
- UDP socket with `SO_REUSEPORT` for config reload
- Per-stream request handling via channels to adapt H/3 to H/2 format
- Alt-SVC header advertises HTTP/3 availability to clients
- **Limitation**: Client certificate authentication not supported for HTTP/3

## Routing and Load Balancing

### Path-Based Routing
- **Longest prefix match** algorithm on URL paths
- Multiple `[[apps.app_name.reverse_proxy]]` entries per app
- Entry without `path` serves as default (catches unmatched requests)
- `replace_path` option rewrites matched path portion

Example:
```toml
[[apps.app.reverse_proxy]]
path = '/api/v1'
replace_path = '/v1'  # /api/v1/users → /v1/users at upstream
```

### Load Balancing Strategies

| Strategy | Selection | Session Persistence | Implementation |
|----------|-----------|---------------------|----------------|
| `none` (default) | Always first | N/A | Index 0 |
| `round_robin` | Sequential rotation | Optional | AtomicUsize counter |
| `random` | Uniform random | No | `rand::rng()` |
| `sticky` | Cookie-based affinity | Yes | SHA256 hash with encrypted cookie |

**Sticky session** implementation:
- Hashes upstream URI + index with SHA256
- Generates encrypted cookie mapping to specific upstream
- Rotates cookie on each response for security
- Falls back to random if cookie invalid/missing

### Automatic Upstream Failover

**NEW**: rpxy now supports automatic failover when upstreams return errors, similar to nginx's `proxy_intercept_errors`.

**Configuration** (`config-failover-example.toml` for examples):
```toml
[[apps.app.reverse_proxy]]
upstream = [
  { location = 'elixir-backend:4000', tls = false },
  { location = 'ruby-backend:3000', tls = false },
]
failover_on_statuses = [404, 502, 503]      # Optional, default: [502, 503, 504]
failover_on_connection_failure = true       # Optional, default: true
max_failover_retries = 1                    # Optional, default: upstream.len() - 1
```

**How It Works**:
1. Load balancer selects initial upstream (respecting load_balance strategy)
2. Request is sent to initial upstream
3. If response matches `failover_on_statuses` or connection fails (and `failover_on_connection_failure` is true), retry with next upstream
4. Upstreams are tried sequentially starting from load balancer's selection
5. Process repeats until success or `max_failover_retries` exhausted
6. Last response (even if error) is returned to client

**Request Body Buffering**:
- Request bodies <1MB are automatically buffered in memory for retries
- Larger bodies skip buffering → failover disabled for that request
- GET/HEAD requests always support failover (no body)
- Body buffering uses efficient channel-based `IncomingLike` type

**Limitations**:
- WebSocket and HTTP/2 upgrade requests: failover automatically disabled (stateful)
- Non-idempotent methods (POST, PUT, etc.): User responsibility (like nginx)
- Large request bodies (>1MB): failover disabled for that specific request
- Retry adds latency on failures (by design)

**Use Cases**:
- **Gradual migration**: Try new backend first, fallback to legacy on 404/501
- **High availability**: Automatically retry on service errors (502, 503, 504)
- **Canary deployments**: Failover from canary to stable on errors
- **Multi-region**: Try local backend, failover to remote on connection failure

**Interaction with Load Balancing**:
- Failover and load balancing are orthogonal and work together
- Load balancer picks the *first* upstream to try
- Failover handles *retries* when that upstream fails
- Sticky sessions are preserved during failover attempts
- Example: `round_robin` + failover = distributed load + error resilience

**Implementation** (`rpxy-lib/src/backend/failover.rs`, `message_handler/handler_main.rs:154`):
- `FailoverConfig`: Per-route failover settings
- `FailoverContext`: Tracks which upstreams have been tried
- Request buffering: `buffer_request_body()` and `reconstruct_request()`
- Retry loop: `request_with_failover()` method
- Integration: Conditional logic in `handle_request_inner()`

## Feature Flags

### HTTP/3 Backend (mutually exclusive)
- `http3-quinn` (default): Quinn-based HTTP/3
- `http3-s2n`: AWS s2n-quic-based HTTP/3

### Optional Features
- `cache`: Response caching with LRU + file storage
- `sticky-cookie`: Session persistence via cookies
- `acme`: Automatic certificate management
- `post-quantum`: Post-quantum key exchange (`X25519MLKEM768`)
- `native-tls-backend`: System TLS for upstream connections
- `rustls-backend` (default): Rustls for upstream
- `webpki-roots`: System CA roots for upstream TLS verification

## Important Implementation Details

### SNI-Host Consistency
By default, `rpxy` validates that the SNI in TLS ClientHello matches the HTTP Host header, preventing domain fronting attacks. This is **not** guaranteed by NGINX by default.

### Private Key Format
All private keys must be in **PKCS8 format**. Use OpenSSL to convert PKCS1:
```bash
openssl pkcs8 -topk8 -nocrypt -in server.key -out server_pkcs8.key
```

### Upstream TLS
Enable TLS to upstream backend:
```toml
reverse_proxy = [{ upstream = [{ location = 'backend:8080', tls = true }] }]
```

### Upstream Options
Available header manipulation options:
- `set_upstream_host`: Rewrite Host to upstream hostname
- `force_http11_upstream` / `force_http2_upstream`: Force HTTP version (mutually exclusive)
- `upgrade_insecure_requests`: Add Upgrade-Insecure-Requests header
- `forwarded_header`: Add RFC 7239 Forwarded header

## Common Development Patterns

### Adding a New Feature
1. Determine which workspace member owns the feature
2. Add feature flag to appropriate `Cargo.toml` if optional
3. Update configuration structs in `rpxy-lib/src/globals.rs` if config needed
4. Implement feature logic in appropriate module
5. Update `config-example.toml` with example configuration
6. Run `cargo fmt` and `cargo test`

### Debugging Request Flow
Enable trace logging to follow request path:
```bash
RUST_LOG=trace cargo run -- --config config.toml
```

Key trace points:
- `message_handler::handler::handle_request`: Request routing
- `forwarder::forwarder::forward`: Upstream forwarding
- `backend::load_balance`: Load balancer selection

### Working with HTTP/3
- HTTP/3 requires both `quinn`/`s2n-quic` and `h3` dependencies
- QUIC connections use separate UDP socket from TCP
- Stream handling is async and uses channels to convert to hyper Body
- Alt-SVC header on HTTP/2 responses advertises HTTP/3 availability

## Testing

Tests are located in each workspace member:
- `rpxy-lib/src/*/tests.rs`: Unit tests inline with modules
- `rpxy-certs/src/tests/`: Certificate handling tests
- Integration tests verify hot-reloading and multi-protocol support

Run tests before submitting PRs to ensure nothing breaks.

## Performance Considerations

- `rpxy` uses Arc-based shared state (`Globals`) for zero-copy sharing
- Request counting with `AtomicUsize` for max client limits
- Lazy TLS handshake reduces overhead for SNI extraction
- Response caching (when enabled) uses LRU + file storage for large responses
- Connection pooling for upstream HTTP/2 connections

## Project Maintenance

This project is maintained on a **best-effort basis** by the original author. See `CONTRIBUTING.md` for contribution guidelines. Security issues should be reported via GitHub's private vulnerability reporting, not public issues.
