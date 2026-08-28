//! The listener: twirp façades and the blob endpoints, one router.
//!
//! ```text
//! POST /twirp/github.actions.results.api.v1.ArtifactService/<Method>
//!        JSON in proto-field-name form (the toolkit serializes with
//!        `useProtoFieldName`), Bearer auth against the exact run token.
//! PUT  /blob/upload/<id>?sig=…&comp=block&blockid=…    stage one block
//! PUT  /blob/upload/<id>?sig=…&comp=blocklist          commit, in list order
//! PUT  /blob/upload/<id>?sig=…                         single-shot content
//! GET  /blob/download/<id>?sig=…                       content (+Range)
//! ```
//!
//! The blob half is the subset of the Azure block-blob protocol the toolkit's
//! `@azure/storage-blob` client actually speaks: `stageBlock` PUTs, one
//! `commitBlockList` XML, streamed GETs. Signed URLs carry an HMAC over
//! `<kind>/<id>` under a per-run secret, so a URL is exactly as capable as the
//! service that minted it. The URL's host half echoes the request's `Host`
//! header — whatever address the client reached the twirp call on is the
//! address its blob calls can reach too.

use std::convert::Infallible;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hmac::{Hmac, Mac};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::store::ArtifactStore;
use crate::token;

type Body = BoxBody<Bytes, io::Error>;
type HmacSha256 = Hmac<Sha256>;

pub(crate) struct Backend {
    token: String,
    key: [u8; 32],
    port: u16,
    artifacts: ArtifactStore,
}

impl Backend {
    pub(crate) fn new(artifacts_dir: PathBuf, port: u16) -> io::Result<Self> {
        Ok(Self {
            token: token::mint(),
            key: token::random(),
            port,
            artifacts: ArtifactStore::open(artifacts_dir)?,
        })
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    fn sign(&self, message: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("any key length works");
        mac.update(message.as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn verify(&self, message: &str, sig: &str) -> bool {
        let Ok(sig) = decode_hex(sig) else {
            return false;
        };
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("any key length works");
        mac.update(message.as_bytes());
        mac.verify_slice(&sig).is_ok()
    }
}

/// The server: a single-threaded runtime on the caller's thread, alive until
/// `shutdown` fires. Dropping the runtime aborts in-flight connections — the
/// run is over, its steps are done.
pub(crate) fn serve(
    listener: std::net::TcpListener,
    backend: Arc<Backend>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the object service");
    rt.block_on(async move {
        let listener =
            tokio::net::TcpListener::from_std(listener).expect("the bound listener registers");
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let backend = Arc::clone(&backend);
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |req| {
                            handle(Arc::clone(&backend), req)
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });
}

async fn handle(
    backend: Arc<Backend>,
    req: Request<Incoming>,
) -> Result<Response<Body>, Infallible> {
    let path = req.uri().path().to_string();
    let response = if let Some(method) =
        path.strip_prefix("/twirp/github.actions.results.api.v1.ArtifactService/")
    {
        let method = method.to_string();
        twirp(&backend, &method, req).await
    } else if let Some(rest) = path.strip_prefix("/blob/") {
        let rest = rest.to_string();
        blob(&backend, &rest, req).await
    } else {
        Ok(plain(StatusCode::NOT_FOUND, "not found"))
    };
    Ok(response.unwrap_or_else(|e| plain(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())))
}

// ── The twirp half ────────────────────────────────────────────────────────

/// A twirp error: the spec's JSON body with its mapped HTTP status.
struct Twirp {
    status: StatusCode,
    code: &'static str,
    msg: String,
}

impl Twirp {
    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            msg: msg.into(),
        }
    }

    fn invalid(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_argument",
            msg: msg.into(),
        }
    }
}

async fn twirp(
    backend: &Backend,
    method: &str,
    req: Request<Incoming>,
) -> io::Result<Response<Body>> {
    let bearer = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if bearer != Some(backend.token()) {
        return Ok(twirp_error(&Twirp {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthenticated",
            msg: "the run token is required".into(),
        }));
    }
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| format!("127.0.0.1:{}", backend.port));
    let body = req
        .into_body()
        .collect()
        .await
        .map_err(io::Error::other)?
        .to_bytes();
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(e) => {
            return Ok(twirp_error(&Twirp::invalid(format!(
                "the request is not JSON: {e}"
            ))));
        }
    };
    let result = match method {
        "CreateArtifact" => create_artifact(backend, &host, &request),
        "FinalizeArtifact" => finalize_artifact(backend, &request),
        "ListArtifacts" => list_artifacts(backend, &request),
        "GetSignedArtifactURL" => signed_artifact_url(backend, &host, &request),
        "DeleteArtifact" => delete_artifact(backend, &request),
        other => Err(Twirp {
            status: StatusCode::NOT_FOUND,
            code: "bad_route",
            msg: format!("no such method: ArtifactService/{other}"),
        }),
    };
    Ok(match result {
        Ok(value) => json_response(StatusCode::OK, &value),
        Err(error) => twirp_error(&error),
    })
}

/// A required string field, in the proto-field-name form the toolkit sends.
fn field<'v>(request: &'v Value, name: &str) -> Result<&'v str, Twirp> {
    request[name]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Twirp::invalid(format!("`{name}` is required")))
}

fn create_artifact(backend: &Backend, host: &str, request: &Value) -> Result<Value, Twirp> {
    let name = field(request, "name")?;
    let id = backend.artifacts.begin(name).map_err(store_error)?;
    Ok(json!({
        "ok": true,
        "signed_upload_url": blob_url(backend, host, "upload", id),
    }))
}

fn finalize_artifact(backend: &Backend, request: &Value) -> Result<Value, Twirp> {
    let name = field(request, "name")?;
    // `hash` is a `StringValue`, which serializes to its bare string.
    let digest = request["hash"].as_str().map(str::to_string);
    let artifact = backend
        .artifacts
        .finalize(name, digest)
        .map_err(store_error)?
        .ok_or_else(|| Twirp::not_found(format!("no upload to finalize for `{name}`")))?;
    Ok(json!({ "ok": true, "artifact_id": artifact.id.to_string() }))
}

fn list_artifacts(backend: &Backend, request: &Value) -> Result<Value, Twirp> {
    let name_filter = request["name_filter"].as_str();
    let id_filter = request["id_filter"].as_str().and_then(|s| s.parse::<i64>().ok());
    let artifacts: Vec<Value> = backend
        .artifacts
        .list()
        .into_iter()
        .filter(|a| name_filter.is_none_or(|name| a.name == name))
        .filter(|a| id_filter.is_none_or(|id| a.id == id))
        .map(|a| {
            let mut entry = json!({
                "workflow_run_backend_id": token::WORKFLOW_RUN_BACKEND_ID,
                "workflow_job_run_backend_id": token::JOB_RUN_BACKEND_ID,
                "database_id": a.id.to_string(),
                "name": a.name,
                "size": a.size.to_string(),
                "created_at": a.created_at,
            });
            if let Some(digest) = a.digest {
                entry["digest"] = json!(digest);
            }
            entry
        })
        .collect();
    Ok(json!({ "artifacts": artifacts }))
}

fn signed_artifact_url(backend: &Backend, host: &str, request: &Value) -> Result<Value, Twirp> {
    let name = field(request, "name")?;
    let artifact = backend
        .artifacts
        .find(name)
        .ok_or_else(|| Twirp::not_found(format!("no artifact named `{name}`")))?;
    Ok(json!({ "signed_url": blob_url(backend, host, "download", artifact.id) }))
}

fn delete_artifact(backend: &Backend, request: &Value) -> Result<Value, Twirp> {
    let name = field(request, "name")?;
    let id = backend
        .artifacts
        .delete(name)
        .map_err(store_error)?
        .ok_or_else(|| Twirp::not_found(format!("no artifact named `{name}`")))?;
    Ok(json!({ "ok": true, "artifact_id": id.to_string() }))
}

fn store_error(e: io::Error) -> Twirp {
    Twirp {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "internal",
        msg: e.to_string(),
    }
}

fn blob_url(backend: &Backend, host: &str, kind: &str, id: i64) -> String {
    let sig = backend.sign(&format!("{kind}/{id}"));
    format!("http://{host}/blob/{kind}/{id}?sig={sig}")
}

// ── The blob half ─────────────────────────────────────────────────────────

async fn blob(backend: &Backend, rest: &str, req: Request<Incoming>) -> io::Result<Response<Body>> {
    let Some((kind, id)) = rest.split_once('/') else {
        return Ok(plain(StatusCode::NOT_FOUND, "not found"));
    };
    let Ok(id) = id.parse::<i64>() else {
        return Ok(plain(StatusCode::NOT_FOUND, "not found"));
    };
    let query = parse_query(req.uri().query().unwrap_or(""));
    let signed = query
        .iter()
        .find(|(k, _)| k == "sig")
        .is_some_and(|(_, sig)| backend.verify(&format!("{kind}/{id}"), sig));
    if !signed {
        return Ok(plain(StatusCode::FORBIDDEN, "bad signature"));
    }
    match (req.method(), kind) {
        (&Method::PUT, "upload") => upload(backend, id, &query, req).await,
        (&Method::GET | &Method::HEAD, "download") => download(backend, id, req).await,
        _ => Ok(plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")),
    }
}

async fn upload(
    backend: &Backend,
    id: i64,
    query: &[(String, String)],
    req: Request<Incoming>,
) -> io::Result<Response<Body>> {
    let comp = query
        .iter()
        .find(|(k, _)| k == "comp")
        .map(|(_, v)| v.as_str());
    match comp {
        // One block of a staged upload.
        Some("block") => {
            let Some((_, block_id)) = query.iter().find(|(k, _)| k == "blockid") else {
                return Ok(plain(StatusCode::BAD_REQUEST, "blockid is required"));
            };
            let dir = backend.artifacts.staging_dir(id);
            tokio::fs::create_dir_all(&dir).await?;
            let path = dir.join(encode_hex(block_id.as_bytes()));
            body_to_file(req.into_body(), &path).await?;
            Ok(empty(StatusCode::CREATED))
        }
        // The commit: concatenate the staged blocks in list order.
        Some("blocklist") => {
            let xml = req
                .into_body()
                .collect()
                .await
                .map_err(io::Error::other)?
                .to_bytes();
            let ids = block_list(&String::from_utf8_lossy(&xml));
            let staging = backend.artifacts.staging_dir(id);
            let target = backend.artifacts.content_path(id);
            let mut out = tokio::fs::File::create(&target).await?;
            for block in &ids {
                let block_path = staging.join(encode_hex(block.as_bytes()));
                let mut file = match tokio::fs::File::open(&block_path).await {
                    Ok(file) => file,
                    Err(_) => {
                        drop(out);
                        let _ = tokio::fs::remove_file(&target).await;
                        return Ok(plain(StatusCode::BAD_REQUEST, "unknown block in the list"));
                    }
                };
                tokio::io::copy(&mut file, &mut out).await?;
            }
            out.flush().await?;
            drop(out);
            let _ = tokio::fs::remove_dir_all(&staging).await;
            Ok(empty(StatusCode::CREATED))
        }
        // A single-shot block-blob PUT: the body is the whole content.
        None => {
            body_to_file(req.into_body(), &backend.artifacts.content_path(id)).await?;
            Ok(empty(StatusCode::CREATED))
        }
        Some(other) => Ok(plain(
            StatusCode::BAD_REQUEST,
            &format!("unsupported comp `{other}`"),
        )),
    }
}

async fn download(
    backend: &Backend,
    id: i64,
    req: Request<Incoming>,
) -> io::Result<Response<Body>> {
    let path = backend.artifacts.content_path(id);
    let total = match tokio::fs::metadata(&path).await {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(plain(StatusCode::NOT_FOUND, "no such content"));
        }
        Err(e) => return Err(e),
    };
    let range = req
        .headers()
        .get(hyper::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_range(v, total));
    let (status, start, len) = match range {
        Some((start, end)) => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        None => (StatusCode::OK, 0, total),
    };
    let mut response = Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_LENGTH, len)
        .header(hyper::header::CONTENT_TYPE, "application/octet-stream")
        .header(hyper::header::ACCEPT_RANGES, "bytes");
    if let Some((s, e)) = range {
        response = response.header(
            hyper::header::CONTENT_RANGE,
            format!("bytes {s}-{e}/{total}"),
        );
    }
    let body = if req.method() == Method::HEAD {
        full(Bytes::new())
    } else {
        file_body(path, start, len)
    };
    Ok(response.body(body).expect("a valid response"))
}

/// `bytes=a-b` (inclusive) or `bytes=a-`; anything else is served whole.
fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let end: u64 = match end {
        "" => total.checked_sub(1)?,
        end => end.parse().ok()?,
    };
    (start <= end && end < total).then_some((start, end))
}

// ── Bodies and small codecs ───────────────────────────────────────────────

async fn body_to_file(mut body: Incoming, path: &Path) -> io::Result<()> {
    let mut file = tokio::fs::File::create(path).await?;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(io::Error::other)?;
        if let Some(data) = frame.data_ref() {
            file.write_all(data).await?;
        }
    }
    file.flush().await
}

/// Stream `len` bytes of `path` from `start`, without holding the file in
/// memory: a reader task feeds frames through a small channel.
fn file_body(path: PathBuf, start: u64, len: u64) -> Body {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, io::Error>>(8);
    tokio::spawn(async move {
        if let Err(e) = read_into(&path, start, len, &tx).await {
            let _ = tx.send(Err(e)).await;
        }
    });
    BoxBody::new(ChannelBody(rx))
}

async fn read_into(
    path: &Path,
    start: u64,
    len: u64,
    tx: &tokio::sync::mpsc::Sender<Result<Frame<Bytes>, io::Error>>,
) -> io::Result<()> {
    let mut file = tokio::fs::File::open(path).await?;
    if start > 0 {
        file.seek(io::SeekFrom::Start(start)).await?;
    }
    let mut remaining = len;
    while remaining > 0 {
        let chunk = remaining.min(64 * 1024) as usize;
        let mut buffer = vec![0u8; chunk];
        match file.read(&mut buffer).await? {
            0 => break,
            n => {
                buffer.truncate(n);
                remaining -= n as u64;
                if tx.send(Ok(Frame::data(buffer.into()))).await.is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

struct ChannelBody(tokio::sync::mpsc::Receiver<Result<Frame<Bytes>, io::Error>>);

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.0.poll_recv(cx)
    }
}

fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).map_err(io::Error::other).boxed()
}

fn empty(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(full(Bytes::new()))
        .expect("a valid response")
}

fn plain(status: StatusCode, message: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain")
        .body(full(message.to_string()))
        .expect("a valid response")
}

fn json_response(status: StatusCode, value: &Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(full(value.to_string()))
        .expect("a valid response")
}

fn twirp_error(error: &Twirp) -> Response<Body> {
    json_response(
        error.status,
        &json!({ "code": error.code, "msg": error.msg }),
    )
}

/// The block ids of an Azure `commitBlockList` document, in order. `Latest`,
/// `Committed` and `Uncommitted` all resolve to the staged block here.
fn block_list(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('>') else { break };
        let tag = &rest[..close];
        rest = &rest[close + 1..];
        if matches!(tag, "Latest" | "Committed" | "Uncommitted") {
            let end = format!("</{tag}>");
            let Some(stop) = rest.find(&end) else { break };
            out.push(rest[..stop].trim().to_string());
            rest = &rest[stop + end.len()..];
        }
    }
    out
}

/// Split a query string, percent-decoding values (`blockid` is base64 and
/// arrives encoded).
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_hex(text: &str) -> Result<Vec<u8>, ()> {
    if !text.len().is_multiple_of(2) {
        return Err(());
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16).ok_or(())?;
            let lo = (pair[1] as char).to_digit(16).ok_or(())?;
            Ok((hi * 16 + lo) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_lists_keep_document_order() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
            <BlockList><Latest>YmxvY2sx</Latest><Uncommitted>YmxvY2sy</Uncommitted>
            <Latest>YmxvY2sz</Latest></BlockList>"#;
        assert_eq!(block_list(xml), vec!["YmxvY2sx", "YmxvY2sy", "YmxvY2sz"]);
    }

    #[test]
    fn ranges_parse_the_shapes_the_toolkit_sends() {
        assert_eq!(parse_range("bytes=0-99", 200), Some((0, 99)));
        assert_eq!(parse_range("bytes=100-", 200), Some((100, 199)));
        assert_eq!(parse_range("bytes=100-300", 200), None);
        assert_eq!(parse_range("items=0-1", 200), None);
    }

    #[test]
    fn query_values_percent_decode() {
        let query = parse_query("sig=abc&blockid=AAAA%2FBB%3D%3D&comp=block");
        assert_eq!(query[1], ("blockid".into(), "AAAA/BB==".into()));
    }
}
