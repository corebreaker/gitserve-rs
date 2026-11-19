//! The HTTP server: the smart HTTP protocol of Git, routed to gunnar-wire.

use std::io::{Cursor, Read};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use gunnar_wire::receive_pack::{AllowAll, ReceivePack};
use gunnar_wire::upload_pack::UploadPack;
use gunnar_wire::{ProtocolVersion, Session};
use storage_git_gix::GixStore;
use tokio::net::TcpListener;

use crate::ingest::StoreIngest;
use crate::repository::{self, LookupError};
use crate::store::Store;

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
/// `<prefix><name>/git-upload-pack` and `<prefix><name>/git-receive-pack`.
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
    let version = headers.get("git-protocol").and_then(|value| value.to_str().ok());
    Ok(GitRequest {
        name: name.to_string(),
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
    store: Store,
    prefix: String,
}

/// Returns the router serving the repositories of `store` under the URL `prefix`, which starts and ends with a
/// slash (see [`normalize_prefix`]).
pub fn router(store: Store, prefix: String) -> Router {
    Router::new()
        .fallback(handle)
        .with_state(Arc::new(AppState { store, prefix }))
        .layer(middleware::from_fn(log_request))
}

/// Serves `router` on `listener` until serving fails.
pub async fn serve(listener: TcpListener, router: Router) -> std::io::Result<()> {
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await
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
        Vec::new()
    } else {
        match read_body(body, &parts.headers).await {
            Ok(body) => body,
            Err(err) => {
                log::warn!(
                    "Reading a request for {} {}: {err:#}",
                    request.service.as_str(),
                    request.name
                );
                return (StatusCode::BAD_REQUEST, "cannot read the request").into_response();
            }
        }
    };

    let task_state = Arc::clone(&state);
    let task_request = request.clone();
    let served = tokio::task::spawn_blocking(move || {
        let store = &task_state.store;
        let run = |dir: &Path| run(dir, &task_request, &body);
        // Pushing changes the repository, everything else only reads it.
        if task_request.service == Service::ReceivePack && !task_request.advertise {
            store.change(&task_request.name, run)
        } else {
            store.view(&task_request.name, run)
        }
    })
    .await;

    let (output, result) = match served {
        Ok(Ok(served)) => served,
        Ok(Err(LookupError::InvalidName(_) | LookupError::NotFound(_))) => {
            return (StatusCode::NOT_FOUND, "repository not found").into_response();
        }
        Err(err) => {
            log::error!("{} {}: {err}", request.service.as_str(), request.name);
            return (StatusCode::INTERNAL_SERVER_ERROR, "the request failed").into_response();
        }
    };

    if let Err(err) = result {
        log::error!("{} {}: {err:#}", request.service.as_str(), request.name);
        if output.is_empty() {
            // Nothing to send yet: the failure can still show as an HTTP status.
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

/// Reads the whole body of a request, decompressing it when it is gzipped.
async fn read_body(body: Body, headers: &HeaderMap) -> anyhow::Result<Vec<u8>> {
    let bytes = to_bytes(body, usize::MAX).await?;
    let gzipped = headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding.as_bytes().eq_ignore_ascii_case(b"gzip"));
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
