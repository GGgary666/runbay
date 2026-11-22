use anyhow::Result;
use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fs;
use std::sync::Arc;
use runbay_gateway::{MockUpstreamServer, SecretlessLoopbackGateway};
use runbay_mvcc::{MVCCWorkspaceStore, SharedOutOfSandboxDerivedProxy};
use runbay_sandbox::{run_os_sandbox_verification_suite, LocalOsSandbox};
use runbay_scheduler::{CancelToken, GuardedRunSink};
use tempfile::TempDir;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("self-check") => run_self_check().await,
        Some("serve") => {
            let port: u16 = args
                .windows(2)
                .find(|w| w[0] == "--port")
                .and_then(|w| w[1].parse().ok())
                .unwrap_or(7310);
            run_loopback_server(port).await
        }
        Some("--version") | Some("-V") => {
            println!("runbayd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            println!(
                "runbayd {} — local runtime daemon\n\nUsage:\n  runbayd self-check          Verify OS sandbox, MVCC/CoW branching, gateway, and cancel guard\n  runbayd serve [--port PORT] Start loopback control server (default 7310)",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
    }
}

async fn run_self_check() -> Result<()> {
    let tmp = TempDir::new()?;
    let src = tmp.path().join("src");
    fs::create_dir_all(&src)?;
    fs::write(src.join("doc_0.md"), "# Core\n\nRunbay MVCC self-check canary.\n")?;

    let store = MVCCWorkspaceStore::new(tmp.path().join("mvcc"), &src)?;
    let proxy = SharedOutOfSandboxDerivedProxy::new(
        tmp.path().join("proxy"),
        Arc::clone(&store),
        32,
        256,
    )?;
    let lease0 = store.pin_snapshot(Some(0))?;
    let mut up = BTreeMap::new();
    up.insert("doc_1.md".into(), "# Delta\n\nNew rev1 content.\n".into());
    store.commit_delta(&up, &[], Some(0))?;
    let rev0_future = proxy.query_at_revision(0, "rev1 content", 3)?;
    let rev1_new = proxy.query_at_revision(1, "rev1 content", 3)?;
    drop(lease0);

    let eq = proxy.verify_incremental_equals_full_rebuild(&["canary", "rev1 content"])?;

    let tok = CancelToken::new();
    let sink = GuardedRunSink::new(tok.clone());
    sink.emit(serde_json::json!({"ok": true}));
    tok.cancel("self_check");
    sink.emit(serde_json::json!({"late": true}));

    let mock = MockUpstreamServer::start().await?;
    let gw = SecretlessLoopbackGateway::start(mock.base_url(), "self-check-host-key", None).await?;
    let run_tok = gw.mint_run_token("self-check", None);
    let resp = reqwest::Client::new()
        .post(format!("{}/chat/completions", gw.base_url()))
        .header("Authorization", format!("Bearer {run_tok}"))
        .json(&serde_json::json!({"model": "self-check", "stream": false}))
        .send()
        .await?;
    let gw_ok = resp.status().is_success();

    let probe_sb = LocalOsSandbox::new(tmp.path(), &[]);
    let sb_ok = if probe_sb.is_available() {
        run_os_sandbox_verification_suite(&tmp.path().join("sb"))?.all_invariants_passed
    } else {
        false
    };

    let summary = serde_json::json!({
        "runbayd_version": env!("CARGO_PKG_VERSION"),
        "mvcc_isolation_ok": rev0_future.hits.is_empty() && !rev1_new.hits.is_empty(),
        "incremental_equals_full": eq.exact_equivalence,
        "cancel_guard_ok": sink.accepted_count() == 1 && sink.rejected_late_count() == 1,
        "secretless_gateway_ok": gw_ok,
        "os_sandbox_ok": sb_ok
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

async fn run_loopback_server(port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let local = listener.local_addr()?;
    println!("runbayd listening on http://{local}");
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        tokio::spawn(async move {
            let svc = service_fn(|req: Request<Incoming>| async move {
                let path = req.uri().path();
                let (status, body) = match path {
                    "/health" => (StatusCode::OK, serde_json::json!({"status": "ok"})),
                    "/v1/status" => (
                        StatusCode::OK,
                        serde_json::json!({
                            "service": "runbayd",
                            "version": env!("CARGO_PKG_VERSION"),
                            "components": ["runbay-mvcc", "runbay-sandbox", "runbay-gateway", "runbay-scheduler"]
                        }),
                    ),
                    _ => (
                        StatusCode::NOT_FOUND,
                        serde_json::json!({"error": "not_found"}),
                    ),
                };
                let resp: Response<BoxBody<Bytes, Infallible>> = Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(body.to_string())).boxed())
                    .unwrap();
                Ok::<_, Infallible>(resp)
            });
            let _ = http1::Builder::new().serve_connection(io, svc).await;
        });
    }
}
