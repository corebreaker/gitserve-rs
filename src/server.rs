//! The HTTP server: the smart HTTP protocol of Git, routed to gunnar-wire.

use std::io::{Cursor, Read};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::body::{Bytes, to_bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use gunnar_wire::receive_pack::{AllowAll, ReceivePack};
use gunnar_wire::upload_pack::UploadPack;
use gunnar_wire::{ProtocolVersion, Session};
use percent_encoding::percent_decode_str;
use storage_git_gix::GixStore;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::ingest::StoreIngest;
use crate::repository;
use crate::store::{AccessError, Store};

/// A Git service, which the client names in the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// Fetches and clones.
    UploadPack,
    /// Pushes.
    ReceivePack,
}

impl Service {
    /// The name of the service in URLs and content types.
    pub fn as_str(self) -> &'static str {
        match self {
            Service::UploadPack => "git-upload-pack",
            Service::ReceivePack => "git-receive-pack",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [Service::UploadPack, Service::ReceivePack]
            .into_iter()
            .find(|service| service.as_str() == name)
    }
}

/// A request for a Git service on a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRequest {
    /// The name under which the repository is served.
    pub name: String,
    /// The service that the client asks for.
    pub service: Service,
    /// Whether the client only asks for the references of the repository, as the first step of a fetch or a push.
    pub advertise: bool,
    /// The version of the protocol that the client asks for, in its `Git-Protocol` header.
    pub version: ProtocolVersion,
}

/// Why a request is not a request of the smart HTTP protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// The path names no endpoint of the protocol.
    NotFound,
    /// The endpoint takes another method, which `Allow` gives.
    MethodNotAllowed(Method),
    /// The client asks for references without naming a service, as clients of the dumb protocol do.
    DumbProtocol,
}

impl IntoResponse for RouteError {
    fn into_response(self) -> Response {
        match self {
            RouteError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            RouteError::MethodNotAllowed(allowed) => (
                StatusCode::METHOD_NOT_ALLOWED,
                [(header::ALLOW, allowed.to_string())],
                "method not allowed",
            )
                .into_response(),
            RouteError::DumbProtocol => (
                StatusCode::FORBIDDEN,
                "only the smart HTTP protocol of Git is supported",
            )
                .into_response(),
        }
    }
}

/// Returns `prefix` with a leading and a trailing slash, the form that [`router`] expects.
pub fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        format!("/{trimmed}/")
    }
}

/// Routes a request to the Git service it asks for. A repository answers at `<prefix><name>/info/refs`,
/// `<prefix><name>/git-upload-pack` and `<prefix><name>/git-receive-pack`, where the name is percent-encoded.
pub fn route(prefix: &str, method: &Method, uri: &Uri, headers: &HeaderMap) -> Result<GitRequest, RouteError> {
    let path = uri.path().strip_prefix(prefix).ok_or(RouteError::NotFound)?;
    let (name, service, advertise) = if let Some(name) = path.strip_suffix("/info/refs") {
        (name, query_service(uri.query()), true)
    } else if let Some((name, endpoint)) = path.rsplit_once('/') {
        let service = Service::from_name(endpoint).ok_or(RouteError::NotFound)?;
        (name, Some(service), false)
    } else {
        return Err(RouteError::NotFound);
    };

    let expected = if advertise { Method::GET } else { Method::POST };
    if method != expected {
        return Err(RouteError::MethodNotAllowed(expected));
    }

    let service = service.ok_or(RouteError::DumbProtocol)?;
    // The decoded name may hold any character: `check_name` validates it before it reaches the file system.
    let name = percent_decode_str(name)
        .decode_utf8()
        .map_err(|_| RouteError::NotFound)?;
    let version = headers.get("git-protocol").and_then(|value| value.to_str().ok());
    Ok(GitRequest {
        name: name.into_owned(),
        service,
        advertise,
        version: ProtocolVersion::from_git_protocol(version),
    })
}

/// Returns the service that the query of an `info/refs` request names.
fn query_service(query: Option<&str>) -> Option<Service> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("service="))
        .and_then(Service::from_name)
}

/// The state shared by the handlers.
struct AppState {
    store: Arc<Store>,
    prefix: String,
}

/// Returns the router serving the repositories of `store` under the URL `prefix`, which starts and ends with a
/// slash (see [`normalize_prefix`]).
pub fn router(store: Arc<Store>, prefix: String) -> Router {
    Router::new()
        .fallback(handle)
        .with_state(Arc::new(AppState { store, prefix }))
        .layer(middleware::from_fn(log_request))
}

/// How long the requests in flight get to complete once a shutdown starts.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Serves `router` on `listener` until `shutdown` completes, then shuts the server down gracefully: it stops accepting
/// connections, and gives the requests in flight `grace` to complete. It returns once they did or `grace` elapsed,
/// or as soon as serving fails on its own.
///
/// The connections still open after `grace` are left to the runtime, which closes them when it is dropped: axum runs
/// each connection in a task of its own. The blocking tasks, such as a push in progress, complete before the runtime
/// is dropped, even if their response can no longer be sent.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    grace: Duration,
) -> std::io::Result<()> {
    let (stop, stopped) = oneshot::channel::<()>();
    let server = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            // A dropped sender stops the server as well.
            let _ = stopped.await;
        });
    // Dropping the future of `serve` before it completes, in a `select!` for instance, stops the server too.
    let mut server = AbortOnDrop(tokio::spawn(async move { server.await }));

    tokio::select! {
        served = &mut server.0 => return served.map_err(std::io::Error::other)?,
        () = shutdown => {}
    }

    let _ = stop.send(());
    match tokio::time::timeout(grace, &mut server.0).await {
        Ok(served) => served.map_err(std::io::Error::other)?,
        Err(_) => {
            log::warn!(
                "Requests in flight outlasted {} s: their connections are closed",
                grace.as_secs_f32()
            );
            Ok(())
        }
    }
}

/// A task that is aborted when dropped.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Logs every request with the status code of its response.
async fn log_request(request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(|| "unknown".to_string(), |ConnectInfo(addr)| addr.to_string());
    let line = format!("[From:{peer}] {} {}", request.method(), request.uri());
    let response = next.run(request).await;
    log::info!("{line} ({})", response.status().as_u16());
    response
}

/// Serves a request of the smart HTTP protocol.
///
/// The whole request is read before the repository is locked, and the response is sent once it is unlocked:
/// a slow client never keeps a repository locked, which would stall the other requests.
async fn handle(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let request = match route(&state.prefix, &parts.method, &parts.uri, &parts.headers) {
        Ok(request) => request,
        Err(err) => return err.into_response(),
    };

    let body = if request.advertise {
        Bytes::new()
    } else {
        match to_bytes(body, usize::MAX).await {
            Ok(body) => body,
            Err(err) => {
                log::warn!(
                    "Reading a request for {} {}: {err}",
                    request.service.as_str(),
                    request.name
                );
                return (StatusCode::BAD_REQUEST, "cannot read the request").into_response();
            }
        }
    };

    let gzipped = parts
        .headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding.as_bytes().eq_ignore_ascii_case(b"gzip"));
    let task_state = Arc::clone(&state);
    let task_request = request.clone();
    let served = tokio::task::spawn_blocking(move || {
        // Decompressing is CPU-bound work too, kept off the async runtime.
        let body = match decode_body(body, gzipped) {
            Ok(body) => body,
            Err(err) => return Served::UnreadableBody(err),
        };

        let store = &task_state.store;
        let access = |dir: &Path| run(dir, &task_request, &body);
        // Pushing changes the repository, everything else only reads it.
        let accessed = if task_request.service == Service::ReceivePack && !task_request.advertise {
            store
                .change(&task_request.name, access)
                .map(|changed| match changed.saved {
                    Ok(()) => changed.value,
                    Err(err) => discard_push(&body, err),
                })
        } else {
            store.view(&task_request.name, access)
        };

        match accessed {
            Ok((output, result)) => Served::Ran { output, result },
            Err(AccessError::Lookup(_)) => Served::NotFound,
            Err(AccessError::Load(err)) => Served::Failed(err),
            Err(AccessError::Closed) => Served::Closed,
        }
    })
    .await;

    let (output, result) = match served {
        Ok(Served::Ran { output, result }) => (output, result),
        Ok(Served::NotFound) => return (StatusCode::NOT_FOUND, "repository not found").into_response(),
        Ok(Served::Closed) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "the server is shutting down").into_response();
        }
        Ok(Served::Failed(err)) => {
            log::error!("{} {}: {err:#}", request.service.as_str(), request.name);
            return (StatusCode::INTERNAL_SERVER_ERROR, "the request failed").into_response();
        }
        Ok(Served::UnreadableBody(err)) => {
            log::warn!(
                "Reading a request for {} {}: {err:#}",
                request.service.as_str(),
                request.name
            );
            return (StatusCode::BAD_REQUEST, "cannot read the request").into_response();
        }
        Err(err) => {
            log::error!("{} {}: {err}", request.service.as_str(), request.name);
            return (StatusCode::INTERNAL_SERVER_ERROR, "the request failed").into_response();
        }
    };

    if let Err(err) = result {
        log::error!("{} {}: {err:#}", request.service.as_str(), request.name);
        // A failed fetch or push may have reported the failure to the client in its output, which must then be
        // sent. A failed advertisement has nothing to report: what it wrote is only its start.
        if request.advertise || output.is_empty() {
            return (StatusCode::INTERNAL_SERVER_ERROR, "the request failed").into_response();
        }
    }

    let kind = if request.advertise { "advertisement" } else { "result" };
    let content_type = format!("application/x-{}-{kind}", request.service.as_str());
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        output,
    )
        .into_response()
}

/// What the blocking part of [`handle`] gives back.
enum Served {
    /// The gzipped body of the request cannot be decompressed.
    UnreadableBody(anyhow::Error),
    /// No repository is served under the name of the request.
    NotFound,
    /// The store cannot be accessed.
    Failed(anyhow::Error),
    /// The store is closed, as the server shuts down.
    Closed,
    /// The service ran: its output, and whether it succeeded.
    Ran {
        output: Vec<u8>,
        result: anyhow::Result<()>,
    },
}

/// Returns the response to a push that the store could not record, and discarded: the error, which makes the push
/// fail on the client, rather than the report of a push that looks done.
///
/// With a side-band channel, the error goes on its error band, which the client displays as a `remote:` message;
/// without one, the response is left empty, for the handler to answer with an HTTP error.
fn discard_push(body: &[u8], err: anyhow::Error) -> (Vec<u8>, anyhow::Result<()>) {
    let err = err.context("the push could not be saved, and was discarded");
    let output = if requests_side_band(body) {
        error_band(&format!("gitserve: {err:#}"))
    } else {
        Vec::new()
    };
    (output, Err(err))
}

/// Reports whether the push request in `body` asks for a side-band channel, among the capabilities that follow
/// a NUL byte on its first packet line.
fn requests_side_band(body: &[u8]) -> bool {
    let length = body
        .get(..4)
        .and_then(|hex| std::str::from_utf8(hex).ok())
        .and_then(|hex| usize::from_str_radix(hex, 16).ok());
    let Some(line) = length.and_then(|length| body.get(4..length)) else {
        return false;
    };
    let Some(nul) = line.iter().position(|&byte| byte == 0) else {
        return false;
    };

    String::from_utf8_lossy(&line[nul + 1..])
        .split_whitespace()
        .any(|capability| matches!(capability, "side-band" | "side-band-64k"))
}

/// Returns `message` on the error band of a side-band channel, followed by a flush packet. The message is cut to
/// fit a packet of the side-band, the smallest of both variants.
fn error_band(message: &str) -> Vec<u8> {
    const MAX_MESSAGE: usize = 1000 - 5;
    let mut end = message.len().min(MAX_MESSAGE);
    while !message.is_char_boundary(end) {
        end -= 1;
    }

    let mut packet = format!("{:04x}", end + 5).into_bytes();
    packet.push(3);
    packet.extend_from_slice(&message.as_bytes()[..end]);
    packet.extend_from_slice(b"0000");
    packet
}

/// Returns the content of a request body, decompressing it when it is gzipped.
fn decode_body(bytes: Bytes, gzipped: bool) -> anyhow::Result<Vec<u8>> {
    if !gzipped {
        return Ok(bytes.into());
    }

    let mut content = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..])
        .read_to_end(&mut content)
        .context("cannot decompress the request")?;
    Ok(content)
}

/// Runs the service that `request` asks for on the repository in `dir`, with the request `body`.
///
/// It returns what the service wrote, even when it fails: a service reports some failures to the client in its
/// response, which must then be sent.
fn run(dir: &Path, request: &GitRequest, body: &[u8]) -> (Vec<u8>, anyhow::Result<()>) {
    let mut output = Vec::new();
    let result = run_service(dir, request, body, &mut output);
    (output, result)
}

fn run_service(dir: &Path, request: &GitRequest, body: &[u8], output: &mut Vec<u8>) -> anyhow::Result<()> {
    let hash_kind = repository::hash_kind(dir)?;
    let store = GixStore::open_or_init(dir, hash_kind)?;
    let mut input = Cursor::new(body);

    match (request.service, request.advertise) {
        (Service::UploadPack, true) => {
            // Protocol v2 has no service announcement.
            if request.version == ProtocolVersion::V0 {
                write_service_announcement(output, request.service);
            }
            UploadPack::new(&store, hash_kind).advertise(request.version, output)?;
        }
        (Service::ReceivePack, true) => {
            // Pushes only have protocol v0.
            write_service_announcement(output, request.service);
            ReceivePack::new(&store, hash_kind).write_ref_advertisement(output)?;
        }
        (Service::UploadPack, false) => {
            UploadPack::new(&store, hash_kind).serve_session(
                request.version,
                Session::StatelessRpc,
                &mut input,
                output,
            )?;
        }
        (Service::ReceivePack, false) => {
            let ingest = StoreIngest::new(&store, hash_kind);
            ReceivePack::new(&store, hash_kind).serve_session(
                Session::StatelessRpc,
                &mut input,
                output,
                &ingest,
                &AllowAll,
            )?;
        }
    }

    Ok(())
}

/// Writes the `# service=` packet line and the flush packet that start an advertisement of protocol v0.
fn write_service_announcement(output: &mut Vec<u8>, service: Service) {
    let line = format!("# service={}\n", service.as_str());
    output.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
    output.extend_from_slice(line.as_bytes());
    output.extend_from_slice(b"0000");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn route_to(method: Method, uri: &str, version: Option<&str>) -> Result<GitRequest, RouteError> {
        let mut headers = HeaderMap::new();
        if let Some(version) = version {
            headers.insert("git-protocol", HeaderValue::from_str(version).unwrap());
        }
        route("/git/", &method, &uri.parse().unwrap(), &headers)
    }

    fn request(name: &str, service: Service, advertise: bool, version: ProtocolVersion) -> GitRequest {
        GitRequest {
            name: name.to_string(),
            service,
            advertise,
            version,
        }
    }

    #[test]
    fn normalize_prefix_adds_the_slashes() {
        assert_eq!(normalize_prefix(""), "/");
        assert_eq!(normalize_prefix("/"), "/");
        assert_eq!(normalize_prefix("git"), "/git/");
        assert_eq!(normalize_prefix("/a/b/"), "/a/b/");
    }

    #[test]
    fn route_finds_the_service() {
        assert_eq!(
            route_to(Method::GET, "/git/team/repo/info/refs?service=git-upload-pack", None),
            Ok(request("team/repo", Service::UploadPack, true, ProtocolVersion::V0))
        );
        assert_eq!(
            route_to(Method::GET, "/git/repo.git/info/refs?service=git-receive-pack", None),
            Ok(request("repo.git", Service::ReceivePack, true, ProtocolVersion::V0))
        );
        assert_eq!(
            route_to(Method::POST, "/git/repo/git-upload-pack", Some("version=2")),
            Ok(request("repo", Service::UploadPack, false, ProtocolVersion::V2))
        );
        assert_eq!(
            route_to(Method::POST, "/git/repo/git-receive-pack", None),
            Ok(request("repo", Service::ReceivePack, false, ProtocolVersion::V0))
        );
    }

    #[test]
    fn route_rejects_other_paths() {
        for uri in [
            "/other/repo/git-upload-pack",
            "/git/repo/objects/info/packs",
            "/git/git-upload-pack",
            "/git/",
        ] {
            assert_eq!(route_to(Method::POST, uri, None), Err(RouteError::NotFound), "{uri}");
        }
    }

    #[test]
    fn route_checks_the_method() {
        assert_eq!(
            route_to(Method::POST, "/git/repo/info/refs?service=git-upload-pack", None),
            Err(RouteError::MethodNotAllowed(Method::GET))
        );
        assert_eq!(
            route_to(Method::GET, "/git/repo/git-receive-pack", None),
            Err(RouteError::MethodNotAllowed(Method::POST))
        );
    }

    #[test]
    fn route_decodes_the_name() {
        assert_eq!(
            route_to(Method::POST, "/git/my%20repo/git-upload-pack", None),
            Ok(request("my repo", Service::UploadPack, false, ProtocolVersion::V0))
        );
        assert_eq!(
            route_to(
                Method::GET,
                "/git/team/%C3%A9t%C3%A9/info/refs?service=git-upload-pack",
                None
            ),
            Ok(request("team/été", Service::UploadPack, true, ProtocolVersion::V0))
        );
        // A decoded slash is checked with the name, as any other.
        assert_eq!(
            route_to(Method::POST, "/git/..%2Frepo/git-upload-pack", None),
            Ok(request("../repo", Service::UploadPack, false, ProtocolVersion::V0))
        );
        assert_eq!(
            route_to(Method::POST, "/git/%FF/git-upload-pack", None),
            Err(RouteError::NotFound)
        );
    }

    #[test]
    fn decode_body_decompresses_gzip() {
        use std::io::Write as _;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"0000").unwrap();
        let gzipped = Bytes::from(encoder.finish().unwrap());

        assert_eq!(decode_body(Bytes::from_static(b"0000"), false).unwrap(), b"0000");
        assert_eq!(decode_body(gzipped, true).unwrap(), b"0000");
        assert!(decode_body(Bytes::from_static(b"not gzip"), true).is_err());
    }

    #[test]
    fn requests_side_band_reads_the_capabilities() {
        let zero = "0".repeat(40);
        let request = |capabilities: &str| {
            let line = format!("{zero} {zero} refs/heads/main\0{capabilities}\n");
            format!("{:04x}{line}0000", line.len() + 4).into_bytes()
        };

        assert!(requests_side_band(&request("report-status side-band-64k agent=git/2")));
        assert!(requests_side_band(&request("side-band")));
        assert!(!requests_side_band(&request("report-status agent=git/2")));
        assert!(!requests_side_band(b"0000"));
        assert!(!requests_side_band(b""));
        assert!(!requests_side_band(b"zzzz"));
        assert!(!requests_side_band(b"0100short"));
        assert!(!requests_side_band(b"000eno capability"));
    }

    #[test]
    fn a_discarded_push_reports_on_the_side_band_only_when_asked() {
        let zero = "0".repeat(40);
        let request = |capabilities: &str| {
            let line = format!("{zero} {zero} refs/heads/main\0{capabilities}\n");
            format!("{:04x}{line}0000", line.len() + 4).into_bytes()
        };

        let (output, result) = discard_push(&request("side-band-64k"), anyhow::anyhow!("disk full"));
        assert!(String::from_utf8_lossy(&output).contains("disk full"));
        assert!(format!("{:#}", result.unwrap_err()).contains("discarded"));

        let (output, result) = discard_push(&request("report-status"), anyhow::anyhow!("disk full"));
        assert!(output.is_empty());
        assert!(result.is_err());
    }

    #[test]
    fn error_band_is_a_packet_of_the_error_band() {
        assert_eq!(error_band("failed"), b"000b\x03failed0000");
        let long = error_band(&"é".repeat(1000));
        assert_eq!(&long[..4], b"03e7");
        assert_eq!(long.len(), 0x3e7 + 4);
    }

    #[test]
    fn route_refuses_the_dumb_protocol() {
        assert_eq!(
            route_to(Method::GET, "/git/repo/info/refs", None),
            Err(RouteError::DumbProtocol)
        );
        assert_eq!(
            route_to(Method::GET, "/git/repo/info/refs?service=git-other", None),
            Err(RouteError::DumbProtocol)
        );
    }

    #[test]
    fn service_announcement_is_a_packet_line_and_a_flush() {
        let mut output = Vec::new();
        write_service_announcement(&mut output, Service::UploadPack);
        assert_eq!(output, b"001e# service=git-upload-pack\n0000");
    }
}
