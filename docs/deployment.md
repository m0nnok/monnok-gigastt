# Deployment

gigastt is a **local-first server**: it listens on `127.0.0.1:9876` by default and refuses to bind to non-loopback addresses unless you pass `--bind-all` (or set `GIGASTT_ALLOW_BIND_ANY=1`). This is intentional — it prevents accidental public exposure.

For remote access, **terminate TLS and add authentication at a reverse proxy**. The server stays on localhost; the proxy handles the internet boundary.

## Security model

- **Server**: binds localhost only, no TLS, no auth
- **Proxy**: handles HTTPS, authentication, rate limiting, origin validation
- **Network**: proxy talks to server via localhost; proxy faces the internet

This separation keeps gigastt simple and lets you choose your proxy, TLS, and auth strategy.

## Caddy (recommended)

Caddy auto-provisions Let's Encrypt certificates and requires zero manual TLS config.

**Caddyfile:**

```
stt.example.com {
    reverse_proxy 127.0.0.1:9876 {
        transport http {
            versions h1 h2c
        }
        # Forward the real peer address so gigastt's per-IP rate-limiter
        # sees each client individually. `{remote_host}` comes from Caddy's
        # view of the TCP connection — clients cannot spoof it, unlike any
        # `X-Forwarded-For` header they may supply.
        header_up X-Real-IP {remote_host}
        header_up X-Forwarded-For {remote_host}
    }
    basic_auth /* {
        admin {env.CADDY_BASIC_AUTH_HASH}
    }
}
```

**Setup:**

```sh
# Generate bcrypt hash for basic_auth
caddy hash-password
# Enter password, copy the hash

# Export hash as environment variable
export CADDY_BASIC_AUTH_HASH='$2a$14$...'

# Run Caddy
caddy run
```

**Why this works:**
- Caddy auto-provisions Let's Encrypt HTTPS
- Redirects HTTP → HTTPS automatically
- `reverse_proxy` upgrades WebSocket connections without extra config
- `h2c` (HTTP/2 Cleartext) to gigastt; browser talks h1/h2 to Caddy
- `basic_auth` protects both REST and WebSocket
- Loopback Origin (`http://127.0.0.1:9876`) is always allowed at the server; browser request goes to `https://stt.example.com` so no CORS issues

## nginx

**nginx.conf:**

Add this at the top of the `http {}` block:

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    '' close;
}
```

In your server block for `stt.example.com`:

```nginx
server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name stt.example.com;

    ssl_certificate /etc/letsencrypt/live/stt.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/stt.example.com/privkey.pem;

    auth_basic "STT API";
    auth_basic_user_file /etc/nginx/.htpasswd;

    location / {
        proxy_pass http://127.0.0.1:9876;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        # Overwrite, do NOT append. Using $proxy_add_x_forwarded_for would
        # concatenate the client-supplied X-Forwarded-For header, letting a
        # malicious client spoof their source IP and bypass the per-IP
        # rate-limiter (`--rate-limit-per-minute`). We want gigastt to see
        # the real peer address only.
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;

        # Long-audio settings — see "Long audio behind a proxy" below.
        client_max_body_size     300m;
        client_body_timeout      300s;
        proxy_max_temp_file_size 1024m;
        proxy_connect_timeout    60s;
        proxy_send_timeout       3600s;
        proxy_read_timeout       3600s;
        send_timeout             3600s;
    }

    # Optional: redirect HTTP to HTTPS
    error_page 497 https://$server_name$request_uri;
}

server {
    listen 80;
    listen [::]:80;
    server_name stt.example.com;
    return 301 https://$server_name$request_uri;
}
```

**Certificates (Let's Encrypt + certbot):**

```sh
sudo certbot certonly --webroot -w /var/www/html \
    -d stt.example.com
# Renews automatically with certbot timer
```

**Basic auth (.htpasswd):**

```sh
htpasswd -c /etc/nginx/.htpasswd admin
# Enter password
sudo chmod 644 /etc/nginx/.htpasswd
sudo nginx -t && sudo systemctl reload nginx
```

**Why these settings:**
- `proxy_http_version 1.1` + `Upgrade`/`Connection` headers handle WebSocket upgrade
- `proxy_read_timeout 3600s` — `POST /v1/transcribe` is synchronous and sends
  nothing until the whole transcript is ready, so this is the timeout that
  produces mystery 504s on long files. See below.
- `X-Forwarded-For $remote_addr` (overwrite, not append) — see warning below for the rate-limiter implications
- `$connection_upgrade` map prevents connection pooling on HTTP/1.0

## Long audio behind a proxy

`POST /v1/transcribe` is synchronous: it holds the connection open and emits
nothing until the full transcript is ready. On CPU, an hour of audio realistically
takes 10–30 minutes, so every timeout between the client and gigastt has to be
larger than that or the request dies before the answer exists.

**Server defaults** (all overridable):

| Setting | Default | Meaning |
|---|---|---|
| `--max-audio-duration-s` | 3900 (65 min) | Longest file accepted; rejected during decode with 422 `audio_too_long` |
| `--body-limit-bytes` | 256 MiB | Largest upload; rejected with 413 `payload_too_large` |
| `--max-inference-secs` | 1800 (30 min) | Wall-clock budget per transcription; 504 `inference_timeout` |
| `--pool-checkout-timeout-secs` | 300 | Wait for a free session before 503 + `Retry-After` |
| `--max-concurrent-uploads` | 0 → `pool_size * 2` | Uploads admitted at once; bounds peak RSS |

Clients can read `max_audio_duration_s` and `max_body_bytes` from `GET /v1/models`
and check a file locally instead of uploading it to find out.

**Nginx Proxy Manager** — put this in *Advanced → Custom Nginx Configuration* for
the proxy host:

```nginx
client_max_body_size      300m;   # must be >= --body-limit-bytes
client_body_timeout       300s;
proxy_request_buffering   on;     # nginx default; keep it
proxy_max_temp_file_size  1024m;
proxy_connect_timeout     60s;
proxy_send_timeout        3600s;
proxy_read_timeout        3600s;  # the one that matters
send_timeout              3600s;
```

- `proxy_read_timeout` is an *idle-between-reads* timeout on the upstream socket.
  Because the synchronous endpoint stays silent for the whole job, it must exceed
  `pool_checkout_timeout_secs + max_inference_secs` (300 + 1800 = 2100 s). 3600 s
  leaves headroom.
- Keep `proxy_request_buffering on`. nginx spools the body to a temp file first,
  so the `proxy_read_timeout` clock does not start until the upload finishes and
  a slow uploader cannot occupy a gigastt session slot. The cost is temp-file
  space inside the proxy container (`proxy_max_temp_file_size`).
- NPM's **Block Common Exploits** toggle and the global `client_max_body_size` in
  `/data/nginx/` both override per-host settings. A 413 with an nginx-branded
  HTML body comes from there, not from gigastt.
- **Cloudflare in front of the proxy is a hard blocker**: the 100 s origin
  timeout on Free/Pro plans cannot be raised, so synchronous hour-long
  transcription cannot complete through it.

**Client timeouts.** Most HTTP clients default well below the job length —
.NET's `HttpClient.Timeout` is 100 seconds and covers reading the response body,
so it will always fire on a long file until raised. Set the client deadline
*above* `pool_checkout_timeout_secs + max_inference_secs` so the server's own
503/504 with `Retry-After` reaches the caller instead of a blind client-side
timeout. Send the body with a known length (`Content-Length`) so nginx can reject
an oversized upload from the headers rather than after transferring it all.

**Memory.** Peak RSS is roughly:

```
models (~600 MB) + in_flight × (body_limit_bytes + 4 bytes × 16000 × max_audio_duration_s)
```

With the defaults and a 4-slot pool that is ~2.6 GB, so **provision at least
4 GB**. The `4 × 16000 × duration` term is the decoded 16 kHz buffer (230 MB for
an hour); it no longer scales with the *source* sample rate, because decoding
resamples in fixed windows rather than buffering the whole file at its original
rate.

**Uncompressed stereo is deliberately not covered** by the 256 MiB default: an
hour of 48 kHz stereo WAV is ~659 MiB, and accepting that per request across the
pool is not a defensible default. Downmix or compress client-side:

```sh
ffmpeg -i call.wav -ac 1 -ar 16000 -c:a pcm_s16le call-16k.wav   # ~110 MiB/hour
ffmpeg -i call.wav -ac 1 -c:a libopus -b:a 32k call.opus         # ~14 MiB/hour
```

## Rate-limiter & X-Forwarded-For (V1-11)

When `--rate-limit-per-minute` is enabled, gigastt reads the peer IP from `X-Forwarded-For` (first hop, trimmed), then `X-Real-IP`, then the TCP `ConnectInfo` — see `src/server/rate_limit.rs::extract_client_ip` — so each real client gets its own token bucket instead of hashing every request behind the single proxy IP.

**The proxy is the trust boundary.** A client can put any value they want in an `X-Forwarded-For` header they send you; if the proxy blindly passes that header through (or _appends_ the peer address to the client's forgery), the rate-limiter bucket is keyed on attacker-controlled data and easily bypassed.

Both recipes above **overwrite** the header with the proxy's view of the TCP peer (`$remote_addr` in nginx, `{remote_host}` in Caddy) — never `$proxy_add_x_forwarded_for` or the default Caddy behaviour, which concatenate. Copy the snippets verbatim unless you know you need per-hop tracing.

If you deploy without a proxy (not recommended for public exposure), leave `--rate-limit-per-minute 0` (default). The server-level semaphore (`MAX_CONCURRENT_CONNECTIONS = 4`) is your only limit; it prevents exhaustion but will not keep a single attacker from reconnecting as fast as the kernel allows.

## Origin header and CORS

When a browser at `https://stt.example.com` makes a request through the proxy, it sets `Origin: https://stt.example.com`.

**Default (no action needed):**
Loopback Origins (`http://127.0.0.1:*`, `http://[::1]:*`, `http://localhost:*`) are always allowed. Since your browser talks to the proxy (not directly to the server), you don't need to add the origin to gigastt.

**If you want to talk directly to gigastt** (same machine, `http://localhost:9876`):
```sh
gigastt serve --allow-origin http://localhost:9876
```

**Multiple origins:**
```sh
gigastt serve \
    --allow-origin https://stt.example.com \
    --allow-origin https://app.example.com
```

**Warning:** `--cors-allow-any` disables origin validation (wildcard CORS). Only use for development.

## Docker

The Dockerfile defaults to `--host 0.0.0.0 --bind-all` (allows the Docker bridge). Keep the server port on loopback when binding to the host:

```sh
docker run -p 127.0.0.1:9876:9876 gigastt
```

This publishes port 9876 inside the container to `127.0.0.1:9876` on the host. The proxy (on the host or in another container) connects via the loopback interface.

**Multi-container setup (docker-compose):**

```yaml
version: '3.8'
services:
  gigastt:
    build: .
    ports:
      - "127.0.0.1:9876:9876"
    # No --bind-all needed; container listens on 0.0.0.0:9876
    # Host bridges it to 127.0.0.1:9876

  caddy:
    image: caddy:latest
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data
      - caddy_config:/config
    depends_on:
      - gigastt
    environment:
      - CADDY_BASIC_AUTH_HASH=${CADDY_BASIC_AUTH_HASH}

volumes:
  caddy_data:
  caddy_config:
```

## Health checks

Both proxies can target `http://127.0.0.1:9876/health` for health checks. The `/health` endpoint is exempted from origin validation, so no CORS headers needed.

```sh
curl http://127.0.0.1:9876/health
# {"status":"ok"}
```

## Graceful shutdown & session caps

gigastt drains live WebSocket / SSE sessions on `SIGTERM` so clients receive a `Final` frame + `Close(1001 Going Away)` instead of a TCP reset. Two flags control the behaviour:

- `--max-session-secs N` / `GIGASTT_MAX_SESSION_SECS` (default `3600`). Wall-clock cap per WebSocket session. When exceeded the server emits `Error { code: "max_session_duration_exceeded" }` + `Close(1008 Policy Violation)`. `0` disables the cap (not recommended — a silence-streaming client will hold an inference slot forever).
- `--shutdown-drain-secs N` / `GIGASTT_SHUTDOWN_DRAIN_SECS` (default `10`, clamped to `>= 1`). Grace window after `SIGTERM` during which in-flight sessions may finish. Should comfortably fit inside your orchestrator's termination grace period so the process is not `SIGKILL`ed mid-drain.

### Kubernetes

Set `terminationGracePeriodSeconds` to **at least `shutdown_drain_secs + 5`** so the kubelet doesn't `SIGKILL` before the drain completes. Example (defaults):

```yaml
apiVersion: apps/v1
kind: Deployment
spec:
  template:
    spec:
      # drain (10 s) + safety margin (5 s) + LB deregistration hook (~15 s)
      terminationGracePeriodSeconds: 30
      containers:
        - name: gigastt
          image: ghcr.io/ekhodzitsky/gigastt:latest
          env:
            - name: GIGASTT_SHUTDOWN_DRAIN_SECS
              value: "10"
            - name: GIGASTT_MAX_SESSION_SECS
              value: "3600"
          lifecycle:
            preStop:
              exec:
                # Give the LB a beat to stop routing new traffic before SIGTERM
                command: ["/bin/sh", "-c", "sleep 10"]
```

### docker-compose

```yaml
services:
  gigastt:
    build: .
    # drain (10 s) + safety margin
    stop_grace_period: 15s
    environment:
      GIGASTT_SHUTDOWN_DRAIN_SECS: "10"
      GIGASTT_MAX_SESSION_SECS: "3600"
```

If you observe clients hanging past the cap or not receiving `Final` on deploy, see `docs/runbook.md` for the rollback escape hatches.

## Hardening checklist

- **Bind address:** Keep `--host 127.0.0.1` unless you're running in a container (then use the port binding strategy above).
- **Rate limiting:** Use `--rate-limit-per-minute N` (v0.8.0+) on the server, or rate-limit at the proxy.
- **TLS termination:** Only at the proxy, never expose the server's raw port to the internet.
- **Origin allowlist:** Explicit `--allow-origin` values; never use `--cors-allow-any` in production.
- **Authentication:** At the proxy (Caddy/nginx basic auth, OAuth, JWT, mTLS, etc.).
- **Audit:** Run `cargo audit` and `cargo deny check` in your CI pipeline.

## See also

- [CLI Reference](../README.md#cli-reference) — `--bind-all`, `--allow-origin`, `--cors-allow-any` flags
- [Security](../README.md#security) — server-side security features
