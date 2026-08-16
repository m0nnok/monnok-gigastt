//! HTTP handlers for REST API endpoints.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Json, Response};
use futures_util::StreamExt;
use futures_util::stream::Stream;
use serde::Serialize;
use std::sync::Arc;

use super::metrics::MetricsRegistry;
use super::{RuntimeLimits, pool_retry_after_ms, pool_retry_after_secs};
use crate::error::GigasttError;
use crate::inference::Engine;

const OPENAPI_YAML: &str = include_str!("../../docs/openapi.yaml");

const SWAGGER_UI_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>gigastt API</title>
  <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css">
  <style>
    body { margin: 0; background: #fff; }
    .swagger-ui .topbar { display: none; }
  </style>
</head>
<body>
  <div id="swagger-ui"></div>
  <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
  <script>
    window.onload = function () {
      window.ui = SwaggerUIBundle({
        url: "/openapi.yaml",
        dom_id: "#swagger-ui",
        deepLinking: true,
        displayRequestDuration: true,
        persistAuthorization: true,
        tryItOutEnabled: true
      });
    };
  </script>
</body>
</html>
"##;

/// Shared application state for all handlers. Carries runtime limits so the
/// WebSocket path can enforce configurable frame / idle bounds without
/// re-threading every CLI arg through each handler, plus an optional
/// in-tree `MetricsRegistry` backing the `/metrics` endpoint.
///
/// Also carries a shutdown `CancellationToken` and a `TaskTracker` used to
/// drain in-flight WebSocket / SSE tasks on SIGTERM (V1-03). `axum::serve`'s
/// built-in `with_graceful_shutdown` only tracks the HTTP router; upgraded
/// WebSocket handlers and `spawn_blocking` SSE tasks fall outside that lane
/// and must be drained explicitly.
pub struct AppState {
    pub engine: Arc<Engine>,
    pub limits: RuntimeLimits,
    pub metrics_registry: Option<Arc<MetricsRegistry>>,
    pub shutdown: tokio_util::sync::CancellationToken,
    pub tracker: tokio_util::task::TaskTracker,
}

/// GET /metrics — Prometheus text-format exposition. Returns 404 when the
/// server was started without `--metrics`.
pub async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    match &state.metrics_registry {
        Some(registry) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            registry.render_prometheus(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "metrics endpoint disabled",
                "code": "metrics_disabled",
            })),
        )
            .into_response(),
    }
}

/// GET /openapi.yaml — OpenAPI document for Swagger UI and client generation.
pub async fn openapi_yaml() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/yaml; charset=utf-8")],
        OPENAPI_YAML,
    )
        .into_response()
}

/// GET /swagger, /swagger/, /docs — Swagger UI for the REST API.
pub async fn swagger_ui() -> Html<&'static str> {
    Html(SWAGGER_UI_HTML)
}

/// Health check response.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub model: String,
    pub version: String,
}

/// Model info response.
#[derive(Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub encoder: String,
    pub vocab_size: usize,
    pub sample_rate: u32,
    pub pool_size: usize,
    pub pool_available: usize,
    pub supported_formats: Vec<String>,
    pub supported_rates: Vec<u32>,
    /// Whether speaker diarization is available (feature-gated build + model loaded).
    /// Added in v0.7.0 so clients can probe capabilities via REST instead of
    /// opening a WebSocket just to read the `Ready` frame.
    pub diarization: bool,
    /// Longest audio file this server will accept, in seconds. Lets a client
    /// check duration locally instead of uploading hundreds of MiB to find out.
    pub max_audio_duration_s: f64,
    /// Largest request body this server will accept, in bytes.
    pub max_body_bytes: usize,
}

/// Transcription response.
#[derive(Serialize)]
pub struct TranscribeResponse {
    pub text: String,
    pub words: Vec<crate::inference::WordInfo>,
    pub duration: f64,
}

/// Error response produced by the REST handlers. Using `Response` directly
/// (rather than a `(StatusCode, Json<_>)` tuple) lets timeout paths attach
/// a `Retry-After` header without changing the handler signatures.
type ApiError = Response;

fn api_error(status: StatusCode, msg: &str, code: &str) -> ApiError {
    (
        status,
        Json(serde_json::json!({"error": msg, "code": code})),
    )
        .into_response()
}

/// Map an engine error to `(status, machine-readable code, client-safe message)`.
///
/// Deliberately does **not** render `e` into the message.
/// [`GigasttError::InvalidAudio`]'s `reason` carries the full `anyhow` chain —
/// symphonia's own unbounded error text, and on the file path the filesystem
/// path from `decode_audio_file`'s context. The only values that cross the trust
/// boundary here are the caller's own upload duration and the server's
/// configured cap, both of which the client is entitled to know.
///
/// The full chain still reaches operators via the `tracing::error!` at the call
/// site.
fn classify_engine_error(e: &GigasttError) -> (StatusCode, &'static str, String) {
    match e {
        GigasttError::AudioTooLong {
            observed_s,
            limit_s,
        } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "audio_too_long",
            format!(
                "Audio is {observed_s:.0}s long; this server accepts up to {limit_s:.0}s. \
                 Split the recording or raise --max-audio-duration-s."
            ),
        ),
        GigasttError::InvalidAudio { .. } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_audio",
            "Could not decode the audio. Supported formats: WAV, MP3, M4A/AAC, OGG, FLAC."
                .to_string(),
        ),
        GigasttError::Timeout { .. } => (
            StatusCode::GATEWAY_TIMEOUT,
            "inference_timeout",
            "Transcription exceeded the server's time budget.".to_string(),
        ),
        GigasttError::Inference { .. } | GigasttError::ModelLoad { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "inference_error",
            "Transcription failed due to an internal error.".to_string(),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal server error.".to_string(),
        ),
    }
}

/// Render an engine error as an HTTP response via [`classify_engine_error`].
fn api_engine_error(e: &GigasttError) -> ApiError {
    let (status, code, msg) = classify_engine_error(e);
    api_error(status, &msg, code)
}

/// 503 response for pool-saturation backpressure: carries both the standard
/// `Retry-After` header (seconds, per RFC 9110 §10.2.3) and a machine-readable
/// `retry_after_ms` field in the JSON body so clients on either surface can
/// back off with the same hint.
pub(crate) fn api_timeout_error(limits: &RuntimeLimits) -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(
            header::RETRY_AFTER,
            pool_retry_after_secs(limits).to_string(),
        )],
        Json(serde_json::json!({
            "error": "Server busy, try again later",
            "code": "timeout",
            "retry_after_ms": pool_retry_after_ms(limits),
        })),
    )
        .into_response()
}

/// 503 response for the case where the pool was closed (graceful shutdown
/// in progress). Distinct from `timeout` so clients can decide whether to
/// retry: a closed pool is not coming back, so no `retry_after_ms` hint.
pub(crate) fn api_pool_closed_error() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "Server is shutting down",
            "code": "pool_closed",
        })),
    )
        .into_response()
}

/// GET /health — health check for monitoring and Docker HEALTHCHECK.
pub async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let _ = &state.engine;
    Json(HealthResponse {
        status: "ok".into(),
        model: "gigaam-v3-e2e-rnnt".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    })
}

/// GET /v1/models — list loaded models and capabilities.
pub async fn models(State(state): State<Arc<AppState>>) -> Json<ModelInfo> {
    let engine = &state.engine;
    #[cfg(feature = "diarization")]
    let diarization = engine.has_speaker_encoder();
    #[cfg(not(feature = "diarization"))]
    let diarization = false;
    Json(ModelInfo {
        id: "gigaam-v3-e2e-rnnt".into(),
        name: "GigaAM v3 RNN-T".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        encoder: if engine.is_int8() {
            "int8".into()
        } else {
            "fp32".into()
        },
        vocab_size: engine.vocab_size(),
        sample_rate: 16000,
        pool_size: engine.pool.total(),
        pool_available: engine.pool.available(),
        supported_formats: vec![
            "wav".into(),
            "mp3".into(),
            "m4a".into(),
            "ogg".into(),
            "flac".into(),
        ],
        supported_rates: super::SUPPORTED_RATES.to_vec(),
        diarization,
        max_audio_duration_s: state.limits.max_audio_duration_s,
        max_body_bytes: state.limits.body_limit_bytes,
    })
}

/// POST /v1/transcribe — upload audio file, get full transcript.
///
/// Accepts raw audio body. Supported formats: WAV, MP3, M4A/AAC, OGG, FLAC.
/// Max body size enforced by the axum `DefaultBodyLimit` layer configured
/// from [`RuntimeLimits::body_limit_bytes`] (default 50 MiB).
pub async fn transcribe(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Json<TranscribeResponse>, ApiError> {
    if body.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Empty request body",
            "empty_body",
        ));
    }

    // Defence-in-depth: `DefaultBodyLimit` already rejects oversized bodies
    // before they reach this handler, but a mis-ordered middleware stack or
    // a `Content-Length`-spoofing client could still deliver too many bytes.
    // The explicit 413 keeps the REST contract honest and gives clients a
    // machine-readable `payload_too_large` code alongside the spec-conformant
    // status. Cheap: `Bytes::len()` is a load, not a walk.
    if body.len() > state.limits.body_limit_bytes {
        return Err(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request body exceeds the configured size limit",
            "payload_too_large",
        ));
    }

    // Checkout a session triplet from the pool (blocks if none available).
    // The guard's lifetime is stripped via `into_owned` so the triplet can
    // travel through `spawn_blocking`; the reservation handles checkin.
    let checkout_start = std::time::Instant::now();
    let guard = match tokio::time::timeout(
        std::time::Duration::from_secs(state.limits.pool_checkout_timeout_secs),
        state.engine.pool.checkout(),
    )
    .await
    {
        Ok(Ok(guard)) => guard,
        Ok(Err(_pool_closed)) => return Err(api_pool_closed_error()),
        Err(_timeout) => {
            if let Some(ref reg) = state.metrics_registry {
                reg.counter_inc("gigastt_pool_timeouts_total", vec![], 1);
                reg.histogram_record(
                    "gigastt_pool_checkout_duration_seconds",
                    vec![],
                    checkout_start.elapsed().as_secs_f64(),
                );
            }
            return Err(api_timeout_error(&state.limits));
        }
    };
    if let Some(ref reg) = state.metrics_registry {
        reg.histogram_record(
            "gigastt_pool_checkout_duration_seconds",
            vec![],
            checkout_start.elapsed().as_secs_f64(),
        );
    }
    let (triplet, reservation) = guard.into_owned();

    let engine = state.engine.clone();
    let max_audio_duration_s = state.limits.max_audio_duration_s;
    let deadline = crate::inference::Deadline::after(std::time::Duration::from_secs(
        state.limits.max_inference_secs,
    ));

    let inference_start = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        let mut triplet = triplet;
        // catch_unwind ensures triplet is returned to pool even on panic
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // `body` is an `axum::body::Bytes` (re-export of `bytes::Bytes`):
            // `clone()` is a refcount bump, not a data copy, so the decode
            // path shares the original upload buffer.
            engine.transcribe_bytes_shared_with_limits(
                body,
                &mut triplet,
                max_audio_duration_s,
                deadline,
            )
        }));
        match r {
            Ok(inference_result) => (inference_result, triplet),
            Err(_) => {
                tracing::error!("Panic in REST transcribe — triplet recovered");
                (
                    Err(crate::error::GigasttError::Inference {
                        source: anyhow::anyhow!("Inference thread panicked").into(),
                    }),
                    triplet,
                )
            }
        }
    })
    .await;
    if let Some(ref reg) = state.metrics_registry {
        reg.histogram_record(
            "gigastt_inference_duration_seconds",
            vec![],
            inference_start.elapsed().as_secs_f64(),
        );
    }

    match result {
        Ok((Ok(result), triplet)) => {
            reservation.checkin(triplet);
            if let Some(ref reg) = state.metrics_registry {
                reg.histogram_record("gigastt_audio_duration_seconds", vec![], result.duration_s);
            }
            // The ratio of these two is the real-time factor, which is what
            // sizing decisions for long files hang on.
            tracing::info!(
                audio_s = result.duration_s,
                elapsed_ms = inference_start.elapsed().as_millis() as u64,
                rtf = inference_start.elapsed().as_secs_f64() / result.duration_s.max(f64::EPSILON),
                words = result.words.len(),
                "transcribe_complete"
            );
            Ok(Json(TranscribeResponse {
                text: result.text,
                words: result.words,
                duration: result.duration_s,
            }))
        }
        Ok((Err(e), triplet)) => {
            reservation.checkin(triplet);
            tracing::error!("Transcription error: {e:#}");
            Err(api_engine_error(&e))
        }
        Err(e) => {
            // spawn_blocking task itself failed (e.g., runtime shutdown).
            // Triplet is lost in this branch; reservation is dropped without
            // sending. The pool degrades by one slot.
            tracing::error!("spawn_blocking join error: {e}");
            Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error",
                "internal",
            ))
        }
    }
}

/// POST /v1/transcribe/stream — upload audio file, get SSE stream of partial/final results.
///
/// Real streaming: audio is processed chunk-by-chunk inside `spawn_blocking`,
/// and segments are sent to the SSE stream via an mpsc channel as they are produced.
pub async fn transcribe_stream(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError> {
    if body.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "Empty request body",
            "empty_body",
        ));
    }

    // Defence-in-depth early reject; matches `/v1/transcribe` — see that
    // handler for the rationale.
    if body.len() > state.limits.body_limit_bytes {
        return Err(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request body exceeds the configured size limit",
            "payload_too_large",
        ));
    }

    // Decode audio first (in spawn_blocking since symphonia is blocking).
    // `body` is `axum::body::Bytes`, so the move into the blocking closure is
    // a refcount bump and `decode_audio_bytes_shared` reads the upload
    // buffer in place.
    // The configured cap is passed explicitly so the SSE path honours this
    // server's `RuntimeLimits` rather than the process-wide default.
    let max_audio_duration_s = state.limits.max_audio_duration_s;
    let samples = tokio::task::spawn_blocking(move || {
        crate::inference::audio::decode_audio_bytes_shared_with_limit(body, max_audio_duration_s)
    })
    .await
    .map_err(|e| {
        tracing::error!("spawn_blocking join error: {e}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal server error",
            "internal",
        )
    })?
    .map_err(|e| {
        tracing::error!("Audio decode error: {e:#}");
        // Same classification as `/v1/transcribe`: a file that is merely too
        // long must not be reported as an undecodable format.
        api_engine_error(&crate::inference::classify_decode_error(e))
    })?;

    // Checkout a session triplet from the pool. Strip the lifetime via
    // `into_owned` so the triplet can travel through `spawn_blocking`.
    let checkout_start = std::time::Instant::now();
    let guard = match tokio::time::timeout(
        std::time::Duration::from_secs(state.limits.pool_checkout_timeout_secs),
        state.engine.pool.checkout(),
    )
    .await
    {
        Ok(Ok(guard)) => guard,
        Ok(Err(_pool_closed)) => return Err(api_pool_closed_error()),
        Err(_timeout) => {
            if let Some(ref reg) = state.metrics_registry {
                reg.counter_inc("gigastt_pool_timeouts_total", vec![], 1);
                reg.histogram_record(
                    "gigastt_pool_checkout_duration_seconds",
                    vec![],
                    checkout_start.elapsed().as_secs_f64(),
                );
            }
            return Err(api_timeout_error(&state.limits));
        }
    };
    if let Some(ref reg) = state.metrics_registry {
        reg.histogram_record(
            "gigastt_pool_checkout_duration_seconds",
            vec![],
            checkout_start.elapsed().as_secs_f64(),
        );
    }
    let (triplet, reservation) = guard.into_owned();

    // Create mpsc channel for streaming segments from spawn_blocking to SSE
    let (tx, rx) =
        tokio::sync::mpsc::channel::<Result<crate::inference::TranscriptSegment, String>>(16);

    let engine = state.engine.clone();
    // V1-03: the axum handler future has already returned by the time the
    // SSE stream starts flowing, so `with_graceful_shutdown` can't observe
    // this task. Clone the shutdown token and check it before every chunk
    // so SIGTERM during a long transcription drops cleanly.
    let cancel = state.shutdown.clone();
    let tracker = state.tracker.clone();
    tracker.spawn_blocking(move || {
        let mut triplet = triplet;

        // catch_unwind ensures triplet is returned to pool even on panic
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut stream_state = engine.create_state(false);
            let chunk_size = 16000; // 1 second at 16kHz

            for chunk in samples.chunks(chunk_size) {
                if cancel.is_cancelled() {
                    tracing::info!("SSE transcription cancelled by shutdown");
                    return;
                }
                match engine.process_chunk(chunk, &mut stream_state, &mut triplet) {
                    Ok(segs) => {
                        for seg in segs {
                            if tx.blocking_send(Ok(seg)).is_err() {
                                // Receiver dropped (client disconnected)
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(Err(format!("{e}")));
                        return;
                    }
                }
            }

            // Flush final segment — best-effort; skipped on cancel.
            if !cancel.is_cancelled()
                && let Some(seg) = engine.flush_state(&mut stream_state)
            {
                let _ = tx.blocking_send(Ok(seg));
            }
        }));

        if result.is_err() {
            tracing::error!("Panic in SSE inference task — triplet recovered");
        }

        // Always return triplet to pool (even after panic). Sync `try_send`
        // is safe from a blocking thread; if the pool was closed in the
        // interim the triplet is silently dropped.
        reservation.checkin(triplet);
    });

    // Convert receiver to SSE stream
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|result| {
            let event = match result {
                Ok(seg) => {
                    let msg = if seg.is_final {
                        serde_json::json!({"type": "final", "text": seg.text, "timestamp": seg.timestamp, "words": seg.words})
                    } else {
                        serde_json::json!({"type": "partial", "text": seg.text, "timestamp": seg.timestamp, "words": seg.words})
                    };
                    Event::default().data(msg.to_string())
                }
                Err(_) => {
                    let msg = serde_json::json!({"type": "error", "message": "Transcription failed.", "code": "inference_error"});
                    Event::default().data(msg.to_string())
                }
            };
            Ok(event)
        });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_engine_error_maps_audio_too_long_to_422() {
        let (status, code, msg) = classify_engine_error(&GigasttError::AudioTooLong {
            observed_s: 2400.0,
            limit_s: 1800.0,
        });
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(code, "audio_too_long");
        // Both numbers are actionable and neither is sensitive: one is the
        // caller's own upload, the other is server policy.
        assert!(
            msg.contains("2400"),
            "message should state the length: {msg}"
        );
        assert!(
            msg.contains("1800"),
            "message should state the limit: {msg}"
        );
    }

    #[test]
    fn test_classify_engine_error_maps_invalid_audio_to_422() {
        let (status, code, _) = classify_engine_error(&GigasttError::InvalidAudio {
            reason: "Unsupported audio format".into(),
        });
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(code, "invalid_audio");
    }

    #[test]
    fn test_classify_engine_error_maps_inference_to_500() {
        let (status, code, _) = classify_engine_error(&GigasttError::Inference {
            source: anyhow::anyhow!("onnx blew up").into(),
        });
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "inference_error");
    }

    #[test]
    fn test_classify_engine_error_maps_timeout_to_504() {
        let (status, code, _) = classify_engine_error(&GigasttError::Timeout { elapsed_s: 1801.0 });
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(code, "inference_timeout");
    }

    #[test]
    fn test_error_message_does_not_leak_paths() {
        // `InvalidAudio.reason` carries the full anyhow chain, which on the
        // file path includes the filesystem path from `decode_audio_file`'s
        // context. The client-facing message must never echo it.
        let (_, _, msg) = classify_engine_error(&GigasttError::InvalidAudio {
            reason: "Failed to open audio file: /home/u/secret-recording.wav".into(),
        });
        assert!(!msg.contains("/home/"), "message leaked a path: {msg}");
        assert!(
            !msg.contains("secret-recording"),
            "message leaked a filename: {msg}"
        );

        // Same for model-load failures, which name a path on disk.
        let (_, _, msg) = classify_engine_error(&GigasttError::ModelLoad {
            path: "/opt/models/v3_e2e_rnnt_encoder_int8.onnx".into(),
            source: None,
        });
        assert!(!msg.contains("/opt/"), "message leaked a model path: {msg}");
        assert!(!msg.contains(".onnx"), "message leaked a model file: {msg}");
    }

    #[test]
    fn test_health_response_serialization() {
        let resp = HealthResponse {
            status: "ok".into(),
            model: "test".into(),
            version: "0.3.0".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["model"], "test");
    }

    #[test]
    fn test_transcribe_response_serialization() {
        let resp = TranscribeResponse {
            text: "hello".into(),
            words: vec![],
            duration: 1.5,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["text"], "hello");
        assert_eq!(v["duration"], 1.5);
    }
}
