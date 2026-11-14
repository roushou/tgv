//! Public command/output contracts, using isolated state and synthetic traffic.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    reason = "integration tests are a shell: helpers outside #[test] fail loudly, and mock servers are spawned tasks"
)]

use ren_daemon::{Daemon, config::Config, host::RandomIds};
use ren_recorder::time::{ManualClock, UnixMillis};
use ren_store::{Reader, Store, query::CallQuery};
use serde_json::Value;
use std::process::{Command, Output};
use std::sync::Arc;

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ren"))
        .args(args)
        .env("NO_COLOR", "1")
        .env("TERM", "xterm")
        .output()
        .unwrap()
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn version_names_the_ren_program_and_its_revision() {
    let output = cli(&["--version"]);
    success(&output);
    let build = ren_cli::Cli::build();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("ren {} ({})\n", build.version(), build.revision())
    );
}

#[test]
fn help_and_usage_errors_keep_their_exit_codes() {
    let output = cli(&["--help"]);
    success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
    assert_eq!(cli(&[]).status.code(), Some(2));
    assert_eq!(cli(&["history", "--limit", "0"]).status.code(), Some(2));
}

#[test]
fn dry_runs_create_no_storage_and_leak_no_secret_or_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("state");
    // Local upstream settings a dry run through --gateway never uses.
    let config = temp.path().join("ren.toml");
    std::fs::write(
        &config,
        "codex_base_url = 'https://user:SECRET@example.com'\n",
    )
    .unwrap();
    for harness in ["pi", "opencode", "codex"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ren"))
            .args([
                "--config",
                config.to_str().unwrap(),
                "run",
                harness,
                "--dry-run",
                "--data-dir",
                data.to_str().unwrap(),
                "--gateway",
                "http://127.0.0.1:1",
                "--",
                "PRIVATE_PROMPT",
            ])
            .env("NO_COLOR", "1")
            .env("TERM", "xterm")
            .env("OPENAI_API_KEY", "SECRET")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{harness}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan = json(&output);
        assert_eq!(plan["executable"], harness);
        assert!(plan.get("result").is_none(), "{harness}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("PRIVATE_PROMPT"), "{harness}");
        assert!(!stdout.contains("SECRET"), "{harness}");
        assert!(output.stderr.is_empty(), "{harness}");
        assert!(!data.exists(), "{harness}");
        if harness == "opencode" {
            assert!(
                plan["environment"]["OPENCODE_CONFIG_CONTENT"]
                    .as_str()
                    .unwrap()
                    .contains("http://127.0.0.1:1/anthropic/v1")
            );
        }
    }
}

#[tokio::test]
async fn history_keeps_direct_json_and_missing_ids_fail_on_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let clock = ManualClock::at(UnixMillis::EPOCH);
    let (store, _) = Store::open(temp.path(), &clock, Arc::new(RandomIds))
        .await
        .unwrap();
    store.close().await.unwrap();
    // Each prints the read API's body for the same read.
    for (name, empty) in [
        ("history", serde_json::json!({ "calls": [], "next": null })),
        ("sessions", serde_json::json!({ "sessions": [] })),
    ] {
        let output = cli(&["--data-dir", temp.path().to_str().unwrap(), name]);
        success(&output);
        assert_eq!(json(&output), empty);
        assert!(output.stderr.is_empty());
    }
    let output = cli(&[
        "--data-dir",
        temp.path().to_str().unwrap(),
        "inspect",
        "00000000-0000-4000-8000-000000000001",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("call, session, or body not found"));
}

/// `cli` with HOME pointed at an empty directory, so no saved gateway or
/// `REN_GATEWAY` from the machine running the tests applies.
fn isolated(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ren"))
        .args(args)
        .env("NO_COLOR", "1")
        .env("TERM", "xterm")
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("REN_GATEWAY")
        .output()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn history_through_a_gateway_prints_what_local_storage_prints() {
    use axum::{Router, response::IntoResponse};
    let payload = r#"{"type":"message","model":"fixture","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":3}}"#;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(move || async move {
                ([("content-type", "application/json")], payload).into_response()
            }),
        )
        .await
        .unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let state = temp.path().join("state");
    let mut daemon = Daemon::new(
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            anthropic_base_url: format!("http://{address}"),
            ..Config::default()
        }
        .resolve()
        .unwrap(),
        &state,
        ren_cli::Cli::build(),
    );
    daemon.start().await.unwrap();
    let gateway = format!("http://{}", daemon.address().unwrap());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for _ in 0..2 {
        let response = client
            .post(format!("{gateway}/anthropic/v1/messages"))
            .header(
                "x-claude-code-session-id",
                "00000000-0000-4000-8000-000000000004",
            )
            .body(r#"{"messages":[]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), payload);
    }
    // Capture is committed shortly after the response.
    let data = state.to_str().unwrap();
    for _ in 0..100 {
        let output = isolated(&home, &["--data-dir", data, "history"]);
        if json(&output)["calls"].as_array().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // The same command, from storage and through the gateway, prints the
    // same bytes.
    let both = |args: &[&str]| {
        let local = isolated(&home, &[&["--data-dir", data], args].concat());
        let remote = isolated(&home, &[args, &["--gateway", &gateway]].concat());
        success(&local);
        success(&remote);
        assert_eq!(
            String::from_utf8_lossy(&remote.stdout),
            String::from_utf8_lossy(&local.stdout),
            "{args:?}"
        );
        json(&local)
    };
    let calls = both(&["history"])["calls"].clone();
    assert_eq!(calls.as_array().unwrap().len(), 2);
    let stored = Reader::open(&state, 1)
        .await
        .unwrap()
        .calls(&CallQuery::default())
        .await
        .unwrap();
    assert_eq!(calls, serde_json::to_value(stored.calls).unwrap());
    let id = calls[0]["id"].as_str().unwrap().to_owned();
    both(&["history", "--limit", "1"]);
    let sessions = both(&["sessions"])["sessions"].clone();
    let session = sessions[0]["id"].as_str().unwrap().to_owned();
    let page = both(&["timeline", &session, "--limit", "1"]);
    let after = page["next"].as_str().unwrap().to_owned();
    let last = both(&["timeline", &session, "--limit", "1", "--after", &after]);
    assert_ne!(last["calls"][0]["id"], page["calls"][0]["id"]);
    assert_eq!(last["next"], Value::Null);
    let detail = both(&["inspect", &id]);
    assert_eq!(detail["call"], calls[0]);
    assert_eq!(detail["attempts"][0]["inspection"]["outcome"], "completed");

    // A saved gateway applies without --gateway; --data-dir still means
    // this host.
    let saved = isolated(&home, &["init", "--gateway", &gateway]);
    success(&saved);
    let remote = isolated(&home, &["history"]);
    let local = isolated(&home, &["--data-dir", data, "history"]);
    success(&remote);
    assert_eq!(remote.stdout, local.stdout);
    std::fs::remove_file(home.join(".config/ren/client.toml")).unwrap();

    // Exports are byte for byte the same, and private.
    for direction in ["request", "response"] {
        let local = temp.path().join(format!("{direction}.local"));
        let remote = temp.path().join(format!("{direction}.remote"));
        let output = isolated(
            &home,
            &[
                "--data-dir",
                data,
                "inspect",
                &id,
                "--body",
                direction,
                "--output",
                local.to_str().unwrap(),
            ],
        );
        success(&output);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        success(&isolated(
            &home,
            &[
                "inspect",
                &id,
                "--body",
                direction,
                "--output",
                remote.to_str().unwrap(),
                "--gateway",
                &gateway,
            ],
        ));
        assert_eq!(
            std::fs::read(&remote).unwrap(),
            std::fs::read(&local).unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for export in [&local, &remote] {
                let mode = std::fs::metadata(export).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        }
    }
    assert_eq!(
        std::fs::read(temp.path().join("response.remote")).unwrap(),
        payload.as_bytes()
    );
    // An existing export is never overwritten.
    let export = temp.path().join("response.local");
    let output = isolated(
        &home,
        &[
            "--data-dir",
            data,
            "inspect",
            &id,
            "--body",
            "response",
            "--output",
            export.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read(export).unwrap(), payload.as_bytes());

    // An unknown call fails remotely too, and leaves no export behind.
    let missing = temp.path().join("missing.bin");
    let output = isolated(
        &home,
        &[
            "inspect",
            "33333333-3333-4333-8333-333333333333",
            "--body",
            "request",
            "--output",
            missing.to_str().unwrap(),
            "--gateway",
            &gateway,
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no such record"));
    assert!(!missing.exists());

    // Controls through a gateway carry no token: the daemon wants a
    // Tailscale identity, and the CLI says how to provide one.
    let output = isolated(&home, &["control", "state", "--gateway", &gateway]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Tailscale identity"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // On this host, the token from the data directory still works.
    let config = temp.path().join("ren.toml");
    std::fs::write(
        &config,
        format!("version = 1\nlisten = \"{}\"\n", daemon.address().unwrap()),
    )
    .unwrap();
    let output = isolated(
        &home,
        &[
            "--config",
            config.to_str().unwrap(),
            "--data-dir",
            data,
            "control",
            "state",
        ],
    );
    success(&output);
    assert_eq!(json(&output)["state"]["pause"]["state"], "running");

    daemon.shutdown();
    daemon.wait().await.unwrap();
    upstream.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_controls_report_the_daemons_answer_and_explain_a_refused_login() {
    use axum::{
        Json, Router,
        http::StatusCode,
        routing::{get, post},
    };
    let app = Router::new()
        .route(
            "/control/v1/state",
            get(|| async {
                Json(serde_json::json!({ "api_version": 1, "state": { "pause": { "state": "running" } }, "running": [] }))
            }),
        )
        .route(
            "/control/v1/pause",
            post(|| async {
                (StatusCode::FORBIDDEN, Json(serde_json::json!({ "api_version": 1, "error": "forbidden" })))
            }),
        )
        .route(
            "/control/v1/calls/{call}/cancel",
            post(|| async {
                (StatusCode::NOT_FOUND, Json(serde_json::json!({ "api_version": 1, "error": "not_running" })))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();

    let output = isolated(home.path(), &["control", "--gateway", &gateway, "state"]);
    success(&output);
    assert_eq!(json(&output)["running"], serde_json::json!([]));

    let output = isolated(home.path(), &["control", "pause", "--gateway", &gateway]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("tailscale_logins"));

    // Other refusals are printed like the local command prints them.
    let output = isolated(
        home.path(),
        &[
            "control",
            "cancel",
            "33333333-3333-4333-8333-333333333333",
            "--gateway",
            &gateway,
        ],
    );
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"], "not_running");
    assert!(String::from_utf8_lossy(&output.stderr).contains("404"));
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn tail_follows_calls_and_resumes_after_a_daemon_restart() {
    use axum::{Router, response::IntoResponse};
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::time::Duration;
    let payload = r#"{"type":"message","model":"fixture","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":2}}"#;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(move || async move {
                ([("content-type", "application/json")], payload).into_response()
            }),
        )
        .await
        .unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    // A fixed port, so the restarted daemon is at the same address.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let config = Config {
        listen: port,
        anthropic_base_url: format!("http://{address}"),
        ..Config::default()
    }
    .resolve()
    .unwrap();
    let state = temp.path().join("state");
    let mut daemon = Daemon::new(config.clone(), &state, ren_cli::Cli::build());
    daemon.start().await.unwrap();
    let gateway = format!("http://{port}");

    let mut tail = std::process::Command::new(env!("CARGO_BIN_EXE_ren"))
        .args(["tail", "--gateway", &gateway])
        .env("NO_COLOR", "1")
        .env("HOME", &home)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (lines, received) = std::sync::mpsc::channel();
    let stdout = tail.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let next = || received.recv_timeout(Duration::from_secs(10)).unwrap();
    let call = || {
        let gateway = gateway.clone();
        async move {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .post(format!("{gateway}/anthropic/v1/messages"))
                .body("{}")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };
    // The feed only sends what happens after tail connects.
    tokio::time::sleep(Duration::from_millis(500)).await;
    call().await;
    let started = next();
    assert!(
        started.contains("  started   ") && started.contains("anthropic messages"),
        "{started}"
    );
    let finished = next();
    assert!(
        finished.contains("  finished  ")
            && finished.contains("anthropic fixture  completed 200  in 7 out 2"),
        "{finished}"
    );

    // Restart: tail reconnects from its last position and misses nothing.
    daemon.shutdown();
    daemon.wait().await.unwrap();
    let mut daemon = Daemon::new(config, &state, ren_cli::Cli::build());
    daemon.start().await.unwrap();
    call().await;
    let started = next();
    assert!(started.contains("  started   "), "{started}");
    assert!(next().contains("  finished  "));

    tail.kill().unwrap();
    tail.wait().unwrap();
    daemon.shutdown();
    daemon.wait().await.unwrap();
    upstream.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_on_this_host_use_the_control_token_and_report_refusals() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let state = temp.path().join("state");
    let root = temp.path().join("work");
    std::fs::create_dir(&root).unwrap();
    let mut daemon = Daemon::new(
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            runs: ren_daemon::config::RunsOptions {
                roots: vec![root.clone()],
                ..ren_daemon::config::RunsOptions::default()
            },
            ..Config::default()
        }
        .resolve()
        .unwrap(),
        &state,
        ren_cli::Cli::build(),
    );
    daemon.start().await.unwrap();
    let config = temp.path().join("ren.toml");
    std::fs::write(
        &config,
        format!("version = 1\nlisten = \"{}\"\n", daemon.address().unwrap()),
    )
    .unwrap();
    let host = |args: &[&str]| {
        isolated(
            &home,
            &[
                &[
                    "--config",
                    config.to_str().unwrap(),
                    "--data-dir",
                    state.to_str().unwrap(),
                    "runs",
                ],
                args,
            ]
            .concat(),
        )
    };

    let output = host(&["ls"]);
    success(&output);
    assert_eq!(json(&output)["runs"], serde_json::json!([]));
    let output = host(&["sessions"]);
    success(&output);
    assert_eq!(json(&output)["sessions"], serde_json::json!([]));
    // A folder outside the roots is refused, and the refusal is printed.
    let outside = temp.path().to_str().unwrap();
    let output = host(&["start", "codex", outside]);
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"], "invalid_directory");
    assert!(String::from_utf8_lossy(&output.stderr).contains("400"));
    let output = host(&["stop", "33333333-3333-4333-8333-333333333333"]);
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"], "not_found");
    let output = host(&["history"]);
    success(&output);
    assert_eq!(json(&output)["runs"], serde_json::json!([]));

    daemon.shutdown();
    daemon.wait().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_through_a_gateway_send_the_control_requests() {
    use axum::{
        Json, Router,
        extract::{Request, State},
        http::StatusCode,
    };
    use std::sync::Mutex;
    type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;
    let seen: Seen = Arc::default();
    let app = Router::new()
        .fallback(
            |State(seen): State<Seen>, request: Request| async move {
                let method = request.method().to_string();
                let path = request.uri().path().to_owned();
                let body = axum::body::to_bytes(request.into_body(), 1 << 20).await.unwrap();
                let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
                seen.lock().unwrap().push((method.clone(), path.clone(), body));
                match (method.as_str(), path.as_str()) {
                    ("POST", "/control/v1/sessions") => (
                        StatusCode::CREATED,
                        Json(serde_json::json!({ "api_version": 1, "session": "11111111-1111-4111-8111-111111111111" })),
                    ),
                    ("GET", "/control/v1/sessions") => (
                        StatusCode::OK,
                        Json(serde_json::json!({ "api_version": 1, "sessions": [] })),
                    ),
                    ("GET", "/control/v1/runs") => (
                        StatusCode::OK,
                        Json(serde_json::json!({ "api_version": 1, "runs": [] })),
                    ),
                    _ => (StatusCode::ACCEPTED, Json(serde_json::json!({ "api_version": 1 }))),
                }
            },
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let session = "11111111-1111-4111-8111-111111111111";
    let remote = |args: &[&str]| {
        isolated(
            home.path(),
            &[&["runs", "--gateway", &gateway], args].concat(),
        )
    };

    let output = remote(&[
        "start",
        "codex",
        "/srv/acme",
        "--resume",
        "22222222-2222-4222-8222-222222222222",
        "--prompt",
        "fix the failing test",
    ]);
    success(&output);
    assert_eq!(json(&output)["session"], session);
    // Through a gateway, a relative folder would name this host's: refused
    // before anything is sent.
    let output = remote(&["start", "codex", "acme"]);
    assert!(!output.status.success());
    success(&remote(&["say", session, "fix", "the", "build"]));
    success(&remote(&["answer", session, "ap-1"]));
    success(&remote(&["answer", session, "ap-2", "decline"]));
    success(&remote(&["interrupt", session]));
    success(&remote(&["stop", session]));
    success(&remote(&["ls"]));
    success(&remote(&["sessions"]));

    let seen = seen.lock().unwrap().clone();
    let expected = [
        (
            "POST",
            "/control/v1/sessions".to_owned(),
            serde_json::json!({ "harness": "codex", "dir": "/srv/acme", "resume": "22222222-2222-4222-8222-222222222222" }),
        ),
        // The first prompt goes to the session made.
        (
            "POST",
            format!("/control/v1/sessions/{session}/prompt"),
            serde_json::json!({ "text": "fix the failing test" }),
        ),
        (
            "POST",
            format!("/control/v1/sessions/{session}/prompt"),
            serde_json::json!({ "text": "fix the build" }),
        ),
        (
            "POST",
            format!("/control/v1/sessions/{session}/approvals/ap-1"),
            serde_json::json!({ "decision": "accept" }),
        ),
        (
            "POST",
            format!("/control/v1/sessions/{session}/approvals/ap-2"),
            serde_json::json!({ "decision": "decline" }),
        ),
        (
            "POST",
            format!("/control/v1/sessions/{session}/interrupt"),
            Value::Null,
        ),
        (
            "POST",
            format!("/control/v1/sessions/{session}/stop"),
            Value::Null,
        ),
        ("GET", "/control/v1/runs".to_owned(), Value::Null),
        ("GET", "/control/v1/sessions".to_owned(), Value::Null),
    ];
    assert_eq!(seen.len(), expected.len(), "{seen:?}");
    for ((method, path, body), (want_method, want_path, want_body)) in seen.iter().zip(expected) {
        assert_eq!(
            (method.as_str(), path, body),
            (want_method, &want_path, &want_body)
        );
    }
    server.abort();
}

#[test]
fn tools_lists_the_catalog_and_refuses_what_it_cannot_change_offline() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("ren.toml");
    std::fs::write(&config, "version = 1\n").unwrap();
    let data = temp.path().join("state");
    let ren = |args: &[&str]| {
        let mut all = vec![
            "--config",
            config.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
            "tools",
        ];
        all.extend(args);
        cli(&all)
    };

    let output = ren(&["list", "--json"]);
    success(&output);
    let listing = json(&output);
    assert_eq!(listing["generation"], 0);
    let ids: Vec<&str> = listing["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "claude", "codex", "pi", "opencode", "node", "bun", "git", "jq", "fd", "ripgrep"
        ]
    );
    let node = &listing["packages"][4];
    assert_eq!(node["kind"], "runtime");
    assert!(node["release"].is_string());
    assert!(node["installed"].is_null());

    // Refused before anything is downloaded.
    let output = ren(&["install", "codex"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("only adopted from the system"));
    let output = ren(&["remove", "jq"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("jq is not installed by Ren"));
    assert_eq!(ren(&["install", "Not-A-Name"]).status.code(), Some(2));

    let output = ren(&["gc"]);
    success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "nothing to remove\n"
    );
}
