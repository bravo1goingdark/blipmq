# BlipMQ Security

BlipMQ's security model is intentionally simple and composable with existing infrastructure:

- **Authentication** via static API keys.
- **Authorization** currently all-or-nothing per key.
- **Transport security** delegated to external components (reverse proxies, service meshes, or TLS terminators).

This document describes the auth model, isolation assumptions, and production deployment security recommendations.

## Static API Key Authentication

`auth` defines:

- `ApiKey(String)`:
  - Immutable wrapper for API key strings.
- `ApiKeyValidator` trait:
  - `fn validate(&self, key: &ApiKey) -> bool`.
- `StaticApiKeyValidator`:
  - Maintains an in-memory `HashSet<ApiKey>` of allowed keys.

`blipmqd`:

- Loads `allowed_api_keys` from config (`blipmq.toml` or YAML) and environment.
- Constructs `StaticApiKeyValidator::from_keys(&config.allowed_api_keys)`.
- Passes it into `net::Server`.

`Connection` in `net`:

- Accepts HELLO and AUTH frames from unauthenticated clients.
- On AUTH:
  - Decodes the API key string from payload.
  - Wraps it into `ApiKey`.
  - Calls `auth_validator.validate(&api_key)`.
  - On success:
    - Sets `authenticated = true`.
    - Sends `ACK`.
  - On failure:
    - Sends `NACK(401, "invalid API key")`.
- For any non-HELLO/AUTH frame before authentication:
  - Sends `NACK(401, "unauthenticated")`.

Implications:

- An empty `allowed_api_keys` list means no clients can authenticate.
- Keys are static for the process lifetime; changing them requires a restart.

## Isolation and Deployment Model

BlipMQ assumes:

- It is deployed on a trusted network segment (e.g. VPC, service mesh).
- Network access to the broker TCP port is restricted to trusted clients.
- TLS/mTLS is handled by:
  - A reverse proxy (Envoy, Nginx, HAProxy).
  - A service mesh (Linkerd, Istio).
  - A sidecar TLS terminator.

BlipMQ itself does not:

- Implement TLS or certificate management.
- Enforce topic-level ACLs.
- Provide built-in rate limiting or quotas.

These aspects should be implemented externally or via future extensions.

## Production Security Recommendations

### 1. Network Access Control

- Bind `bind_addr` to an internal interface or `127.0.0.1` if fronted by a proxy.
- Restrict access to the broker TCP port (e.g. 5555) using:
  - Security groups.
  - Firewalls.
  - Kubernetes network policies.
- Do not expose BlipMQ directly on the public Internet without an authenticating, TLS-terminating proxy.

### 2. API Key Management

- Generate cryptographically strong keys (at least 128 bits of entropy).
- Store keys in a secrets manager (e.g. HashiCorp Vault, AWS Secrets Manager).
- Provision `allowed_api_keys` into BlipMQ via config file or environment at startup:

  ```toml
  allowed_api_keys = [
    "prod-2025-key-A-very-long-random-string",
    "prod-2025-key-B-very-long-random-string",
  ]
  ```

- Rotate keys regularly:
  - Add new key to config.
  - Roll clients to new key.
  - Remove old key and restart BlipMQ.

### 3. Process & File-Level Isolation

Run `blipmqd` as an unprivileged user:

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin blipmq
sudo mkdir -p /etc/blipmq /var/lib/blipmq
sudo chown -R blipmq:blipmq /etc/blipmq /var/lib/blipmq
sudo chmod 750 /etc/blipmq /var/lib/blipmq
sudo chmod 640 /etc/blipmq/blipmq.toml
```

- Ensure WAL files (`wal_path`) are only readable/writable by the `blipmq` user.
- Avoid running BlipMQ as root.

### 4. Secure systemd Unit Example

`/etc/systemd/system/blipmq.service`:

```ini
[Unit]
Description=BlipMQ message broker
After=network.target
Wants=network-online.target

[Service]
User=blipmq
Group=blipmq
ExecStart=/usr/local/bin/blipmqd --config /etc/blipmq/blipmq.toml
Environment=RUST_LOG=info
WorkingDirectory=/var/lib/blipmq

# Restrict filesystem access: read-only system, explicit write dir
ReadWritePaths=/var/lib/blipmq
ProtectSystem=full
ProtectHome=true
PrivateTmp=true
NoNewPrivileges=true

# Lock down Linux capabilities
CapabilityBoundingSet=
AmbientCapabilities=

Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
```

This unit:

- Runs BlipMQ as user `blipmq`.
- Restricts filesystem access to `/var/lib/blipmq`.
- Removes unnecessary capabilities.
- Provides basic restart behavior.

### 5. WAL & Data Protection

- Store WAL on:
  - Encrypted storage (e.g. LUKS, provider-managed disk encryption).
  - A filesystem with proper permissions.
- Consider:
  - Regular backups or snapshots of WAL dir if WAL is part of your recovery strategy.
  - Monitoring for disk health and IO errors.

### 6. Logging & Sensitive Data

- By default, BlipMQ does not log message payloads.
- Avoid adding logs that contain:
  - Full message payloads (which may contain PII or secrets).
  - API keys or other credentials.
- Keep `RUST_LOG` at `info` or `warn` in production; use `debug` or `trace` only during investigations, and ensure logs are properly handled.

### 7. Metrics & Monitoring

- Scrape the `/metrics` endpoint to:
  - Detect unusual patterns (e.g. increasing `messages_inflight`).
  - Correlate with auth failures or WAL errors from logs.
- Set alerts for:
  - High error rates (NACK counts observed via logs or downstream metrics).
  - High inflight messages or stalled deliveries.
  - WAL-related issues (e.g. repeated open failures or corruption).

## Summary

- Authentication is controlled by static API keys; ensure strong key management.
- Transport security and sophisticated authorization should be layered via proxies and network controls.
- Run `blipmqd` under a dedicated user with restricted filesystem privileges.
- Harden logging and monitor metrics to detect and respond to security-relevant events.

