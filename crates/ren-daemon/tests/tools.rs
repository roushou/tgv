//! `/control/v1/tools` end to end: the daemon's toolbox behind the control
//! API, over a tool store in a temporary folder, a catalog of one test
//! release, and downloads served from memory. No network.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    reason = "integration tests are a shell: helpers outside #[test] fail loudly, and mock servers are spawned tasks"
)]

mod support;

use axum::Router;
use ren_daemon::{
    host::HostTools,
    tools::{
        HostToolbox, ToolStore,
        fetch::{ByteLimit, Fetch},
    },
};
use ren_depot::{
    Depot,
    job::HostFailure,
    package::{Kind, Package, PackageId},
    source::{ArchiveFormat, Artifact, ArtifactUrl, Platform, Release, Sha256, Source},
};
use serde_json::{Value, json};
use sha2::Digest as _;
use std::{io::Write as _, path::Path, sync::Arc};
use support::{TestGateway, client, mock};

const URL: &str = "https://example.test/jq-1.8.2.tar.gz";

struct MemoryFetch(Vec<u8>);

impl Fetch for MemoryFetch {
    async fn fetch(&self, url: &ArtifactUrl, into: &Path, _: ByteLimit) -> Result<(), HostFailure> {
        if url.as_str() != URL {
            return Err(HostFailure::Refused);
        }
        std::fs::write(into, &self.0).map_err(|_| HostFailure::Filesystem)
    }
}

fn tarball() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let script = b"#!/bin/sh\necho jq-1.8.2\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(u64::try_from(script.len()).unwrap());
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, "jq-1.8.2/bin/jq", &script[..])
        .unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&builder.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

async fn gateway(dir: &Path) -> (TestGateway, support::Mock) {
    let bytes = tarball();
    let depot = Depot::of(vec![
        Package::new(
            PackageId::JQ,
            Kind::Utility,
            "jq".parse().unwrap(),
            Source::Release(Release {
                version: "1.8.2".parse().unwrap(),
                artifacts: vec![Artifact {
                    platform: Platform::LinuxX64,
                    url: URL.parse().unwrap(),
                    sha256: Sha256::from_bytes(sha2::Sha256::digest(&bytes).into()),
                    format: ArchiveFormat::TarGz,
                    program_at: "jq-1.8.2/bin/jq".parse().unwrap(),
                }],
            }),
        ),
        Package::new(
            PackageId::CODEX,
            Kind::Client,
            "codex".parse().unwrap(),
            Source::System,
        ),
    ]);
    let toolbox = HostToolbox::new(
        ToolStore::new(dir.join("tools"), MemoryFetch(bytes)),
        depot,
        HostTools::new(Vec::new()),
        Some(Platform::LinuxX64),
        None,
    );
    let upstream = mock(Router::new()).await;
    let gateway = TestGateway::with_tools(
        &support::config(&upstream),
        &dir.join("data"),
        Arc::new(toolbox),
    )
    .await;
    (gateway, upstream)
}

async fn call(
    gateway: &TestGateway,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let url = format!("{}{path}", gateway.base());
    let request = match method {
        "GET" => client().get(url),
        _ => client().post(url),
    };
    let request = request.bearer_auth(TestGateway::CONTROL_TOKEN);
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(body.to_string()),
        None => request,
    };
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (
        status,
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap(),
    )
}

#[tokio::test]
async fn lists_installs_removes_and_collects_through_the_control_api() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let (gateway, _upstream) = gateway(dir.path()).await;

    let (status, listing) = call(&gateway, "GET", "/control/v1/tools", None).await;
    assert_eq!(status, 200, "{listing}");
    assert_eq!(listing["api_version"], 1);
    assert_eq!(listing["platform"], "linux_x64");
    assert_eq!(listing["generation"], 0);
    assert_eq!(listing["packages"][0]["id"], "jq");
    assert_eq!(listing["packages"][0]["release"], "1.8.2");
    assert_eq!(listing["packages"][0]["installed"], Value::Null);
    assert_eq!(listing["packages"][1]["needed_by"], json!(["codex"]));

    let (status, changed) = call(
        &gateway,
        "POST",
        "/control/v1/tools/change",
        Some(json!({"add": ["jq"]})),
    )
    .await;
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["outcome"], "activated");
    assert_eq!(changed["generation"], 1);
    assert_eq!(
        changed["changes"],
        json!([{"change": "add", "package": "jq", "from": null, "to": "1.8.2"}])
    );
    let (_, listing) = call(&gateway, "GET", "/control/v1/tools", None).await;
    assert_eq!(listing["packages"][0]["installed"]["version"], "1.8.2");
    assert_eq!(
        listing["generations"],
        json!([{"generation": 1, "state": "current", "installs": [{"package": "jq", "version": "1.8.2"}]}])
    );

    // Refusals name what was wrong, and change nothing.
    for (edit, message) in [
        (
            json!({"add": ["codex"]}),
            "codex is only adopted from the system; Ren does not install it",
        ),
        (json!({"remove": ["fd"]}), "fd is not installed by Ren"),
        (
            json!({"add": ["jq"], "remove": ["jq"]}),
            "jq is both added and removed",
        ),
    ] {
        let (status, body) = call(&gateway, "POST", "/control/v1/tools/change", Some(edit)).await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["error"], "tools_refused");
        assert_eq!(body["message"], message);
    }
    let (status, body) = call(
        &gateway,
        "POST",
        "/control/v1/tools/change",
        Some(json!({"add": ["Bad Name"]})),
    )
    .await;
    assert_eq!((status, &body["error"]), (400, &json!("invalid_request")));

    let (status, changed) = call(
        &gateway,
        "POST",
        "/control/v1/tools/change",
        Some(json!({"remove": ["jq"]})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(changed["generation"], 2);
    let (status, collected) = call(&gateway, "POST", "/control/v1/tools/gc", None).await;
    assert_eq!(status, 200, "{collected}");
    assert_eq!(collected["generations"], json!([1]));
    assert_eq!(collected["installs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn tools_need_the_control_token_and_refuse_browsers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let (gateway, _upstream) = gateway(dir.path()).await;
    let url = format!("{}/control/v1/tools", gateway.base());
    let response = client().get(&url).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 401);
    let response = client()
        .post(format!("{url}/change"))
        .bearer_auth(TestGateway::CONTROL_TOKEN)
        .header("origin", "https://example.test")
        .header("content-type", "application/json")
        .body(json!({"add": ["jq"]}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
    assert!(!dir.path().join("tools/current").exists());
}
