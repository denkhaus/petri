//! The service over real HTTP: the twirp lifecycle and the blob protocol,
//! spoken the way the toolkit speaks them — proto-field-name JSON, staged
//! blocks committed by an XML list, signed URLs used verbatim.
//!
//! The client here is a plain blocking `TcpStream`: the service runs on its
//! own thread, and every response carries a `Content-Length`, so no async
//! machinery is needed to test it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

use github_objects::ObjectService;
use serde_json::{Value, json};

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir()
            .join("petri-objects-e2e")
            .join(format!("{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Reply {
    status: u16,
    body:   Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
    }
}

/// One HTTP/1.1 exchange. `target` is path+query; headers get Host and
/// Content-Length added.
fn exchange(port: u16, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let mut request = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(request.as_bytes()).expect("send head");
    stream.write_all(body).expect("send body");

    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut buffer).expect("read");
        assert!(n > 0, "the server closed before a full response");
        raw.extend_from_slice(&buffer[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .expect("a status")
        .parse()
        .expect("numeric status");
    let content_length: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().to_string())
        })
        .expect("every response carries a length")
        .parse()
        .expect("numeric length");
    let mut body = raw[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut buffer).expect("read body");
        assert!(n > 0, "the server closed mid-body");
        body.extend_from_slice(&buffer[..n]);
    }
    body.truncate(content_length);
    Reply { status, body }
}

fn twirp_call(service: &ObjectService, api: &str, method: &str, request: &Value) -> Reply {
    exchange(
        service.port(),
        "POST",
        &format!("/twirp/github.actions.results.api.v1.{api}/{method}"),
        &[
            ("Authorization", &format!("Bearer {}", service.token())),
            ("Content-Type", "application/json"),
        ],
        request.to_string().as_bytes(),
    )
}

fn twirp(service: &ObjectService, method: &str, request: &Value) -> Reply {
    twirp_call(service, "ArtifactService", method, request)
}

/// Path+query out of a signed URL (the client uses it against the same host).
fn target_of(url: &str) -> String {
    let rest = url.strip_prefix("http://").expect("an http URL");
    let slash = rest.find('/').expect("a path");
    rest[slash..].to_string()
}

#[test]
fn the_artifact_lifecycle_round_trips_over_http() {
    let scratch = Scratch::new("lifecycle");
    let service =
        ObjectService::start(scratch.0.join("artifacts"), scratch.0.join("cache")).expect("start");

    // Create: the toolkit's exact request shape.
    let created = twirp(
        &service,
        "CreateArtifact",
        &json!({
            "workflow_run_backend_id": "petri-wf-run",
            "workflow_job_run_backend_id": "petri-job",
            "name": "dist",
            "version": 4,
        }),
    );
    assert_eq!(created.status, 200);
    let created = created.json();
    assert_eq!(created["ok"], json!(true));
    let upload = created["signed_upload_url"].as_str().expect("a signed URL");

    // Stage two blocks, out of order names, then commit in list order.
    let target = target_of(upload);
    let stage = |id: &str, bytes: &[u8]| {
        let reply = exchange(
            service.port(),
            "PUT",
            &format!("{target}&comp=block&blockid={id}"),
            &[],
            bytes,
        );
        assert_eq!(reply.status, 201, "stageBlock");
    };
    stage("Zmlyc3Q%3D", b"zip-first;");
    stage("c2Vjb25k", b"zip-second");
    let commit = exchange(
        service.port(),
        "PUT",
        &format!("{target}&comp=blocklist"),
        &[],
        b"<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>\
          <Latest>Zmlyc3Q=</Latest><Latest>c2Vjb25k</Latest></BlockList>",
    );
    assert_eq!(commit.status, 201, "commitBlockList");

    let finalized = twirp(
        &service,
        "FinalizeArtifact",
        &json!({ "name": "dist", "size": "20", "hash": "sha256:abcd" }),
    );
    assert_eq!(finalized.status, 200);
    assert_eq!(finalized.json()["ok"], json!(true));

    // List sees it, with the token's backend ids echoed back.
    let listed = twirp(&service, "ListArtifacts", &json!({ "name_filter": "dist" }));
    let listed = listed.json();
    let artifact = &listed["artifacts"][0];
    assert_eq!(artifact["name"], json!("dist"));
    assert_eq!(artifact["size"], json!("20"));
    assert_eq!(artifact["digest"], json!("sha256:abcd"));
    assert_eq!(artifact["workflow_run_backend_id"], json!("petri-wf-run"));

    // Download through the signed URL: the committed order, byte for byte.
    let signed = twirp(&service, "GetSignedArtifactURL", &json!({ "name": "dist" }));
    let url = signed.json()["signed_url"]
        .as_str()
        .expect("a signed URL")
        .to_string();
    let content = exchange(service.port(), "GET", &target_of(&url), &[], b"");
    assert_eq!(content.status, 200);
    assert_eq!(content.body, b"zip-first;zip-second");

    // A range read of the middle.
    let partial = exchange(
        service.port(),
        "GET",
        &target_of(&url),
        &[("Range", "bytes=4-8")],
        b"",
    );
    assert_eq!(partial.status, 206);
    assert_eq!(partial.body, b"first");

    // Delete, and the list is empty.
    let deleted = twirp(&service, "DeleteArtifact", &json!({ "name": "dist" }));
    assert_eq!(deleted.json()["ok"], json!(true));
    let listed = twirp(&service, "ListArtifacts", &json!({}));
    assert_eq!(listed.json()["artifacts"], json!([]));
}

#[test]
fn the_token_is_the_auth_and_signatures_gate_the_blobs() {
    let scratch = Scratch::new("auth");
    let service =
        ObjectService::start(scratch.0.join("artifacts"), scratch.0.join("cache")).expect("start");

    // A wrong bearer is refused with a twirp error.
    let refused = exchange(
        service.port(),
        "POST",
        "/twirp/github.actions.results.api.v1.ArtifactService/ListArtifacts",
        &[("Authorization", "Bearer not-the-token")],
        b"{}",
    );
    assert_eq!(refused.status, 401);
    assert_eq!(refused.json()["code"], json!("unauthenticated"));

    // A blob URL with a bad signature is refused.
    let refused = exchange(
        service.port(),
        "PUT",
        "/blob/upload/1?sig=deadbeef",
        &[],
        b"x",
    );
    assert_eq!(refused.status, 403);

    // An unknown artifact is a twirp not_found.
    let missing = twirp(
        &service,
        "GetSignedArtifactURL",
        &json!({ "name": "ghost" }),
    );
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json()["code"], json!("not_found"));
}

#[test]
fn a_fresh_service_over_the_same_store_serves_prior_artifacts() {
    let scratch = Scratch::new("resume");
    let first =
        ObjectService::start(scratch.0.join("artifacts"), scratch.0.join("cache")).expect("start");
    let created = twirp(
        &first,
        "CreateArtifact",
        &json!({ "name": "kept", "version": 4 }),
    )
    .json();
    let target = target_of(created["signed_upload_url"].as_str().expect("url"));
    let put = exchange(first.port(), "PUT", &target, &[], b"whole-content");
    assert_eq!(put.status, 201, "a single-shot PUT is content too");
    twirp(&first, "FinalizeArtifact", &json!({ "name": "kept" }));
    drop(first);

    // The resume case: new listener, new token, same store.
    let second = ObjectService::start(scratch.0.join("artifacts"), scratch.0.join("cache"))
        .expect("restart");
    let listed = twirp(&second, "ListArtifacts", &json!({})).json();
    assert_eq!(listed["artifacts"][0]["name"], json!("kept"));
    let url = twirp(&second, "GetSignedArtifactURL", &json!({ "name": "kept" })).json();
    let content = exchange(
        second.port(),
        "GET",
        &target_of(url["signed_url"].as_str().expect("url")),
        &[],
        b"",
    );
    assert_eq!(content.body, b"whole-content");
}

/// The cache façade, as the toolkit calls it: reserve, upload, finalize,
/// look up by restore-key prefix, download — and a miss answers `ok: false`
/// with status 200, which is what the client treats as a plain miss.
#[test]
fn the_cache_lifecycle_round_trips_over_http() {
    let scratch = Scratch::new("cache");
    let service =
        ObjectService::start(scratch.0.join("artifacts"), scratch.0.join("cache")).expect("start");
    let cache =
        |method: &str, request: &Value| twirp_call(&service, "CacheService", method, request);

    // A miss first: ok false, status 200.
    let miss = cache(
        "GetCacheEntryDownloadURL",
        &json!({ "key": "deps-abc", "restore_keys": [], "version": "v1" }),
    );
    assert_eq!(miss.status, 200);
    assert_eq!(miss.json()["ok"], json!(false));

    // Reserve, upload (single shot), finalize.
    let reserved = cache(
        "CreateCacheEntry",
        &json!({ "key": "deps-abc", "version": "v1" }),
    )
    .json();
    assert_eq!(reserved["ok"], json!(true));
    let upload = reserved["signed_upload_url"].as_str().expect("a URL");
    let put = exchange(
        service.port(),
        "PUT",
        &target_of(upload),
        &[],
        b"tar-zst-bytes",
    );
    assert_eq!(put.status, 201);
    let finalized = cache(
        "FinalizeCacheEntryUpload",
        &json!({ "key": "deps-abc", "size_bytes": "13", "version": "v1" }),
    )
    .json();
    assert_eq!(finalized["ok"], json!(true));

    // A second reserve of the same pair is refused — immutable, as on GitHub.
    let again = cache(
        "CreateCacheEntry",
        &json!({ "key": "deps-abc", "version": "v1" }),
    )
    .json();
    assert_eq!(again["ok"], json!(false));

    // Restore-key prefix hit, then the bytes.
    let hit = cache(
        "GetCacheEntryDownloadURL",
        &json!({ "key": "deps-zzz", "restore_keys": ["deps-"], "version": "v1" }),
    )
    .json();
    assert_eq!(hit["ok"], json!(true));
    assert_eq!(hit["matched_key"], json!("deps-abc"));
    let url = hit["signed_download_url"].as_str().expect("a URL");
    let content = exchange(service.port(), "GET", &target_of(url), &[], b"");
    assert_eq!(content.body, b"tar-zst-bytes");
}
