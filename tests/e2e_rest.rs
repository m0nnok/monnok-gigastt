//! End-to-end REST API tests for the gigastt HTTP server.
//!
//! All tests require the GigaAM model to be downloaded (~850MB).
//! Run with: `cargo test --test e2e_rest -- --ignored`

mod common;

use futures_util::StreamExt;
use std::time::Duration;

// ---------------------------------------------------------------------------
// 1. Health endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_health_returns_ok() {
    let (port, shutdown) = common::start_server(&common::model_dir()).await;

    let resp = tokio::time::timeout(Duration::from_secs(10), async {
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .expect("GET /health failed")
    })
    .await
    .expect("GET /health timed out");

    assert_eq!(resp.status(), 200);

    let text = resp.text().await.expect("Expected text body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("Expected JSON body");
    assert_eq!(body["status"], "ok", "status field should be \"ok\"");
    assert!(
        body["model"]
            .as_str()
            .unwrap_or_default()
            .contains("gigaam"),
        "model field should contain \"gigaam\", got: {:?}",
        body["model"]
    );
    assert!(
        !body["version"].as_str().unwrap_or_default().is_empty(),
        "version field should be a non-empty string"
    );

    let _ = shutdown.send(());
}

// ---------------------------------------------------------------------------
// 2. POST /v1/transcribe — valid WAV
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_transcribe_wav_returns_text() {
    let (port, shutdown) = common::start_server(&common::model_dir()).await;
    let wav = common::generate_wav(2, 16000);

    let resp = tokio::time::timeout(Duration::from_secs(30), async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/transcribe"))
            .body(wav)
            .send()
            .await
            .expect("POST /v1/transcribe failed")
    })
    .await
    .expect("POST /v1/transcribe timed out");

    assert_eq!(resp.status(), 200);

    let text = resp.text().await.expect("Expected text body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("Expected JSON body");
    assert!(
        body["text"].is_string(),
        "\"text\" field should be a string, got: {:?}",
        body["text"]
    );
    assert!(
        body["words"].is_array(),
        "\"words\" field should be an array, got: {:?}",
        body["words"]
    );
    let duration = body["duration"]
        .as_f64()
        .expect("\"duration\" should be a number");
    assert!(duration > 0.0, "duration should be > 0, got {duration}");

    let _ = shutdown.send(());
}

// ---------------------------------------------------------------------------
// 3. POST /v1/transcribe — empty body → 400
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_transcribe_empty_body_returns_400() {
    let (port, shutdown) = common::start_server(&common::model_dir()).await;

    let resp = tokio::time::timeout(Duration::from_secs(10), async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/transcribe"))
            .body(Vec::<u8>::new())
            .send()
            .await
            .expect("POST /v1/transcribe failed")
    })
    .await
    .expect("POST /v1/transcribe timed out");

    assert_eq!(resp.status(), 400);

    let text = resp.text().await.expect("Expected text body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("Expected JSON body");
    assert_eq!(
        body["code"], "empty_body",
        "code field should be \"empty_body\", got: {:?}",
        body["code"]
    );

    let _ = shutdown.send(());
}

// ---------------------------------------------------------------------------
// 4. POST /v1/transcribe — invalid audio → 422
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_transcribe_invalid_audio_returns_422() {
    let (port, shutdown) = common::start_server(&common::model_dir()).await;

    // 1000 random-ish bytes that are not a valid audio file
    let garbage: Vec<u8> = (0u8..=255).cycle().take(1000).collect();

    let resp = tokio::time::timeout(Duration::from_secs(30), async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/transcribe"))
            .body(garbage)
            .send()
            .await
            .expect("POST /v1/transcribe failed")
    })
    .await
    .expect("POST /v1/transcribe timed out");

    assert_eq!(resp.status(), 422);

    let text = resp.text().await.expect("Expected text body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("Expected JSON body");
    let code = body["code"].as_str().unwrap_or_default();
    assert_eq!(
        code, "invalid_audio",
        "undecodable bytes must be reported as invalid_audio, got: {code:?}"
    );

    let _ = shutdown.send(());
}

// ---------------------------------------------------------------------------
// 4b. Long-audio support (60-minute call workload)
// ---------------------------------------------------------------------------

/// A file past the chunked-transcription threshold must transcribe end to end,
/// and its word timestamps must stay inside the reported duration.
///
/// The timestamp bound is the regression guard for the chunked-offset drift:
/// the old code advanced `frame_offset` by an estimate that ran 40 ms short per
/// 20 s chunk, so a 12-minute file drifted ~1.4 s and the last word landed
/// beyond `duration`.
#[tokio::test]
#[ignore]
async fn test_transcribe_long_audio_returns_full_transcript() {
    let (port, shutdown) = common::start_server(&common::model_dir()).await;

    let duration_s = 12 * 60;
    let wav = common::generate_wav(duration_s, 16000);

    let resp = tokio::time::timeout(Duration::from_secs(1800), async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/transcribe"))
            .body(wav)
            .send()
            .await
            .expect("POST /v1/transcribe failed")
    })
    .await
    .expect("POST /v1/transcribe timed out");

    assert_eq!(resp.status(), 200, "12-minute audio must be accepted");
    let text = resp.text().await.expect("Expected text body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("Expected JSON body");

    let reported = body["duration"].as_f64().expect("duration field");
    assert!(
        (reported - duration_s as f64).abs() < 1.0,
        "reported duration {reported}s should match the {duration_s}s input"
    );

    let words = body["words"].as_array().expect("words field");
    let mut prev_start = f64::NEG_INFINITY;
    for w in words {
        let start = w["start"].as_f64().expect("word start");
        assert!(
            start >= prev_start,
            "word starts must be non-decreasing: {start} after {prev_start}"
        );
        prev_start = start;
    }
    if let Some(last) = words.last() {
        let end = last["end"].as_f64().expect("word end");
        assert!(
            end <= reported + 0.5,
            "last word ends at {end}s, past the {reported}s file — timestamp drift regressed"
        );
    }

    let _ = shutdown.send(());
}

/// Audio past the configured cap must say so, distinctly from "cannot decode".
#[tokio::test]
#[ignore]
async fn test_transcribe_over_limit_returns_audio_too_long() {
    let (port, shutdown) = common::start_server_with_limits(
        &common::model_dir(),
        gigastt::server::RuntimeLimits {
            max_audio_duration_s: 5.0,
            ..Default::default()
        },
    )
    .await;

    let wav = common::generate_wav(10, 16000);

    let resp = tokio::time::timeout(Duration::from_secs(60), async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/transcribe"))
            .body(wav)
            .send()
            .await
            .expect("POST /v1/transcribe failed")
    })
    .await
    .expect("POST /v1/transcribe timed out");

    assert_eq!(resp.status(), 422);
    let text = resp.text().await.expect("Expected text body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("Expected JSON body");
    assert_eq!(
        body["code"].as_str().unwrap_or_default(),
        "audio_too_long",
        "over-length audio must not be reported as a format problem: {body}"
    );
    let msg = body["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains('5'),
        "message should state the configured limit: {msg}"
    );

    let _ = shutdown.send(());
}

/// `GET /v1/models` advertises the limits so clients can pre-check locally.
#[tokio::test]
#[ignore]
async fn test_models_advertises_limits() {
    let (port, shutdown) = common::start_server(&common::model_dir()).await;

    let text = reqwest::get(format!("http://127.0.0.1:{port}/v1/models"))
        .await
        .expect("GET /v1/models failed")
        .text()
        .await
        .expect("Expected text body");
    let body: serde_json::Value = serde_json::from_str(&text).expect("Expected JSON body");

    assert!(
        body["max_audio_duration_s"].as_f64().unwrap_or(0.0) >= 3600.0,
        "server should advertise at least an hour: {body}"
    );
    assert!(
        body["max_body_bytes"].as_u64().unwrap_or(0) > 0,
        "server should advertise its body limit: {body}"
    );

    let _ = shutdown.send(());
}

// ---------------------------------------------------------------------------
// 5. POST /v1/transcribe/stream — SSE stream completes without error
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_transcribe_stream_sse_incremental() {
    let (port, shutdown) = common::start_server(&common::model_dir()).await;
    let wav = common::generate_wav(10, 16000);

    let resp = tokio::time::timeout(Duration::from_secs(60), async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/transcribe/stream"))
            .body(wav)
            .send()
            .await
            .expect("POST /v1/transcribe/stream failed")
    })
    .await
    .expect("POST /v1/transcribe/stream timed out");

    assert_eq!(resp.status(), 200);

    // Collect all SSE bytes — stream should terminate cleanly
    let mut stream = resp.bytes_stream();
    let mut all_bytes = Vec::new();

    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => all_bytes.extend_from_slice(&bytes),
                Err(e) => {
                    eprintln!("SSE stream error: {e}");
                    break;
                }
            }
        }
    })
    .await
    .expect("SSE stream did not complete within 60s");

    // Any data: lines present must be valid JSON with a type field
    let raw = String::from_utf8_lossy(&all_bytes);
    for line in raw.lines() {
        if let Some(json_str) = line.strip_prefix("data:") {
            let json_str = json_str.trim();
            if json_str.is_empty() {
                continue;
            }
            let v: serde_json::Value =
                serde_json::from_str(json_str).expect("SSE data should be valid JSON");
            assert!(
                v["type"].is_string(),
                "SSE event should have a \"type\" field, got: {:?}",
                v
            );
        }
    }

    let _ = shutdown.send(());
}

// ---------------------------------------------------------------------------
// 6. POST /v1/transcribe/stream — empty body → 400
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_transcribe_stream_empty_body_returns_400() {
    let (port, shutdown) = common::start_server(&common::model_dir()).await;

    let resp = tokio::time::timeout(Duration::from_secs(10), async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/transcribe/stream"))
            .body(Vec::<u8>::new())
            .send()
            .await
            .expect("POST /v1/transcribe/stream failed")
    })
    .await
    .expect("POST /v1/transcribe/stream timed out");

    assert_eq!(resp.status(), 400);

    let _ = shutdown.send(());
}

// ---------------------------------------------------------------------------
// 7. SSE events well-formed: data: prefix + valid JSON with type field
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_sse_events_well_formed() {
    let (port, shutdown) = common::start_server(&common::model_dir()).await;
    let wav = common::generate_wav(5, 16000);

    let resp = tokio::time::timeout(Duration::from_secs(60), async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/transcribe/stream"))
            .body(wav)
            .send()
            .await
            .expect("POST /v1/transcribe/stream failed")
    })
    .await
    .expect("POST /v1/transcribe/stream timed out");

    assert_eq!(resp.status(), 200);

    // Collect all SSE bytes
    let mut stream = resp.bytes_stream();
    let mut all_bytes = Vec::new();
    let collect_timeout = Duration::from_secs(30);

    tokio::time::timeout(collect_timeout, async {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => all_bytes.extend_from_slice(&bytes),
                Err(e) => {
                    eprintln!("SSE stream error: {e}");
                    break;
                }
            }
        }
    })
    .await
    .ok(); // timeout is acceptable — stream may still be open

    let raw = String::from_utf8_lossy(&all_bytes);

    // Any data: lines present must be well-formed JSON with a type field.
    // Note: a pure sine wave may produce zero transcription events — that's OK.
    for line in raw.lines() {
        if let Some(json_str) = line.strip_prefix("data:") {
            let json_str = json_str.trim();
            if json_str.is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(json_str)
                .unwrap_or_else(|_| panic!("SSE data line is not valid JSON: {json_str:?}"));
            let event_type = v["type"]
                .as_str()
                .unwrap_or_else(|| panic!("SSE event missing \"type\" field: {v:?}"));
            assert!(
                event_type == "partial" || event_type == "final",
                "SSE event type should be \"partial\" or \"final\", got: {event_type:?}"
            );
        }
    }

    let _ = shutdown.send(());
}

// ---------------------------------------------------------------------------
// 8. Midstream disconnect — server should not panic
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_sse_midstream_disconnect() {
    let (port, shutdown) = common::start_server(&common::model_dir()).await;
    let wav = common::generate_wav(10, 16000);

    let resp = tokio::time::timeout(Duration::from_secs(60), async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/transcribe/stream"))
            .body(wav)
            .send()
            .await
            .expect("POST /v1/transcribe/stream failed")
    })
    .await
    .expect("POST /v1/transcribe/stream timed out");

    assert_eq!(resp.status(), 200);

    // Read just the first event, then drop the response to simulate disconnect
    let mut stream = resp.bytes_stream();
    let _first = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("Timed out waiting for first SSE event");

    // Drop the stream, simulating client disconnect
    drop(stream);

    // Give the server a moment to detect the disconnect and clean up
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Server should still be alive — verify with a /health check
    let health_resp = tokio::time::timeout(Duration::from_secs(10), async {
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .expect("GET /health after disconnect failed")
    })
    .await
    .expect("GET /health after disconnect timed out");

    assert_eq!(
        health_resp.status(),
        200,
        "Server should still be healthy after midstream disconnect"
    );

    let _ = shutdown.send(());
}

// The v0.9.0-rc.1 zero-copy REST decode path (V1-05) used to carry a
// Linux-only VmRSS budget test here. It asserted that
// `RSS_after - RSS_before < wav.len() * 3 + 40 MiB` after POSTing a 300 s
// WAV to `/v1/transcribe`. In practice the full inference pass allocates
// 90+ MiB of encoder scratch alone for 5 minutes of 16 kHz audio, and
// ONNX Runtime keeps the INT8 session state resident — the delta was
// ~320 MiB in CI regardless of whether the upload path did 1× or 4× copies.
// The RSS signal from the upload path itself was drowned out by inference
// cost, so the test could neither catch the regression it was designed to
// catch nor pass reliably. The zero-copy contract is still enforced by the
// `BytesMediaSource` type in `src/inference/audio.rs`, which is covered by
// unit tests and is not exercised by this integration surface.
