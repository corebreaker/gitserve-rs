//! The graceful shutdown of the server: driven through a future in the process, and through real signals on Unix.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use gitserve::repository;
use gitserve::server;
use gitserve::store::Store;
use tokio::sync::{Notify, oneshot};

const WAIT: Duration = Duration::from_secs(10);

/// What the routes of [`test_router`] share with the test.
struct Routes {
    /// Receives the path of each request that a route started to serve.
    started: mpsc::Receiver<&'static str>,
    /// Lets the requests to `/wait` complete.
    release: Arc<Notify>,
}

impl Routes {
    /// Waits until a route started to serve a request to `path`.
    fn wait_started(&self, path: &str) {
        assert_eq!(self.started.recv_timeout(WAIT).unwrap(), path);
    }
}

/// Returns a router whose `/wait` answers `done` once the test releases it, and whose `/hang` never answers.
/// Both tell the test when they start, so that the test knows a request is in flight without guessing a delay.
fn test_router() -> (Router, Routes) {
    let (started, started_receiver) = mpsc::channel();
    let release = Arc::new(Notify::new());
    let (wait_started, hang_started, wait_release) = (started.clone(), started, Arc::clone(&release));
    let router = Router::new()
        .route(
            "/wait",
            get(move || {
                let (started, release) = (wait_started.clone(), Arc::clone(&wait_release));
                async move {
                    let _ = started.send("/wait");
                    release.notified().await;
                    "done"
                }
            }),
        )
        .route(
            "/hang",
            get(move || {
                let started = hang_started.clone();
                async move {
                    let _ = started.send("/hang");
                    std::future::pending::<()>().await;
                    "never"
                }
            }),
        );

    let routes = Routes {
        started: started_receiver,
        release,
    };
    (router, routes)
}

/// A server on `router`, run as the serve command runs it: in a runtime of its own, dropped once the server returned.
struct Running {
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    /// Receives what the server returned, once its runtime is dropped.
    done: mpsc::Receiver<std::io::Result<()>>,
}

impl Running {
    fn start(router: Router, grace: Duration) -> Self {
        let (port_sender, port_receiver) = mpsc::channel();
        let (stop, stopped) = oneshot::channel::<()>();
        let (done_sender, done) = mpsc::channel();
        thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let served = runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                port_sender.send(listener.local_addr().unwrap().port()).unwrap();
                let shutdown = async move {
                    let _ = stopped.await;
                };
                server::serve(listener, router, shutdown, grace).await
            });
            // Dropping the runtime closes the connections left open, as in the serve command.
            drop(runtime);
            let _ = done_sender.send(served);
        });

        Self {
            port: port_receiver.recv().unwrap(),
            stop: Some(stop),
            done,
        }
    }

    /// Starts the shutdown.
    fn stop(&mut self) {
        self.stop.take().unwrap().send(()).unwrap();
    }

    /// Sends `GET path` from a thread of its own, and returns what the thread gets back: the response, or the error
    /// that ended the connection.
    fn get(&self, path: &str) -> mpsc::Receiver<std::io::Result<String>> {
        let (sender, receiver) = mpsc::channel();
        let (port, path) = (self.port, path.to_string());
        thread::spawn(move || {
            let _ = sender.send(request(port, &path));
        });
        receiver
    }
}

/// Sends `GET path` to the server on `port`, and returns the whole response.
fn request(port: u16, path: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

#[test]
fn a_request_in_flight_completes() {
    let (router, routes) = test_router();
    let mut server = Running::start(router, Duration::from_secs(5));
    let response = server.get("/wait");
    routes.wait_started("/wait");
    server.stop();
    routes.release.notify_one();

    let response = response.recv_timeout(WAIT).unwrap().unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("done"), "{response}");
    server.done.recv_timeout(WAIT).unwrap().unwrap();
}

#[test]
fn a_new_connection_is_refused_once_the_shutdown_started() {
    let (router, routes) = test_router();
    let mut server = Running::start(router, Duration::from_secs(5));
    // The request in flight, held until the end of the test, keeps the server shutting down.
    let in_flight = server.get("/wait");
    routes.wait_started("/wait");
    server.stop();

    // The server stops accepting connections soon after the shutdown starts: probe until it refuses one.
    let deadline = Instant::now() + WAIT;
    while let Ok(response) = request(server.port, "/missing") {
        if response.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "new connections are still served");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        in_flight.try_recv().is_err(),
        "the request in flight completed before the probe was refused"
    );

    routes.release.notify_one();
    assert!(in_flight.recv_timeout(WAIT).unwrap().unwrap().ends_with("done"));
    server.done.recv_timeout(WAIT).unwrap().unwrap();
}

#[test]
fn a_request_outlasting_the_grace_is_cut() {
    let (router, routes) = test_router();
    let mut server = Running::start(router, Duration::from_millis(300));
    let response = server.get("/hang");
    routes.wait_started("/hang");
    let started = Instant::now();
    server.stop();

    server.done.recv_timeout(WAIT).unwrap().unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(3), "the shutdown took {elapsed:?}");
    // The connection is closed without a response.
    if let Ok(response) = response.recv_timeout(WAIT).unwrap() {
        assert!(!response.contains("never"), "{response}");
    }
}

#[test]
fn a_closed_store_answers_503() {
    let root = tempfile::tempdir().unwrap();
    repository::create_repository(root.path(), "repo", "main").unwrap();
    let store = Arc::new(Store::new(root.path()));
    let mut server = Running::start(
        server::router(Arc::clone(&store), "/".to_string()),
        Duration::from_secs(5),
    );

    let advertisement = "info/refs?service=git-upload-pack";
    assert!(
        request(server.port, &format!("/repo/{advertisement}"))
            .unwrap()
            .starts_with("HTTP/1.1 200")
    );
    store.close();
    // A closed store gives a 503, whether the repository exists or not.
    for name in ["repo", "missing"] {
        let response = request(server.port, &format!("/{name}/{advertisement}")).unwrap();
        assert!(response.starts_with("HTTP/1.1 503"), "{name}: {response}");
        assert!(response.ends_with("the server is shutting down"), "{name}: {response}");
    }

    server.stop();
    server.done.recv_timeout(WAIT).unwrap().unwrap();
}

/// Waits for `child` to exit, killing it after `WAIT`.
fn wait(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("gitserve did not exit");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn serve_exits_with_1_when_the_port_is_taken() {
    let dir = tempfile::tempdir().unwrap();
    let taken = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
    let port = taken.local_addr().unwrap().port().to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_gitserve"))
        .args(["serve", "-p", &port, "store"])
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert_eq!(wait(&mut child).code(), Some(1));
}

/// A serve command running on the store `store` of a temporary directory, whose temporary files go to its `tmp`
/// directory.
#[cfg(unix)]
struct Served {
    dir: tempfile::TempDir,
    child: Child,
    port: u16,
    /// Receives the lines of the log of the server.
    logged: mpsc::Receiver<String>,
    log: Vec<String>,
}

#[cfg(unix)]
impl Served {
    /// Starts the serve command, and waits until it listens.
    fn start(store: &str) -> Self {
        use std::io::{BufRead, BufReader};

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("tmp")).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_gitserve"))
            .args(["serve", "-p", "0", store])
            .current_dir(dir.path())
            .env("TMPDIR", dir.path().join("tmp"))
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let (lines, logged) = mpsc::channel();
        let stderr = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = lines.send(line);
            }
        });

        let mut served = Self {
            dir,
            child,
            port: 0,
            logged,
            log: Vec::new(),
        };
        let line = served.wait_for("on port ");
        served.port = line
            .split("on port ")
            .nth(1)
            .and_then(|rest| rest.split(',').next()?.parse().ok())
            .unwrap();
        served
    }

    /// Waits for a line of the log that contains `text`, and returns it.
    fn wait_for(&mut self, text: &str) -> String {
        loop {
            let line = self
                .logged
                .recv_timeout(WAIT)
                .unwrap_or_else(|_| panic!("{text:?} was not logged: {:#?}", self.log));
            self.log.push(line.clone());
            if line.contains(text) {
                return line;
            }
        }
    }

    /// Sends `signal` to the server.
    fn signal(&self, signal: &str) {
        let pid = self.child.id().to_string();
        assert!(
            Command::new("kill")
                .args([&format!("-{signal}"), &pid])
                .status()
                .unwrap()
                .success()
        );
    }

    /// The number of entries in the temporary directory of the server.
    fn temporary_entries(&self) -> usize {
        std::fs::read_dir(self.dir.path().join("tmp")).unwrap().count()
    }
}

/// Stops a serve of an archive store with `signal`, and checks that it shuts down: it logs the signal, exits with 0,
/// and removes the image of the archive from the temporary directory.
#[cfg(unix)]
fn stop_with(signal: &str) {
    let mut served = Served::start("repos.zip");
    assert_eq!(served.temporary_entries(), 1, "the image of the archive is missing");
    let response = request(served.port, "/repos/info/refs?service=git-upload-pack").unwrap();
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");

    served.signal(signal);
    let status = wait(&mut served.child);
    served.log.extend(served.logged.try_iter());
    let log = &served.log;
    assert_eq!(status.code(), Some(0), "{log:#?}");
    let expected = format!("Received SIG{signal}, shutting down");
    assert!(log.iter().any(|line| line.contains(&expected)), "{log:#?}");
    assert_eq!(
        served.temporary_entries(),
        0,
        "the image of the archive was left behind"
    );
}

#[cfg(unix)]
#[test]
fn sigint_stops_the_server() {
    stop_with("INT");
}

#[cfg(unix)]
#[test]
fn sigterm_stops_the_server() {
    stop_with("TERM");
}

#[cfg(unix)]
#[test]
fn a_second_signal_exits_at_once() {
    let mut served = Served::start("store");

    // A request whose body never comes stays in flight, which holds the shutdown for its whole grace period.
    let mut stuck = TcpStream::connect(("127.0.0.1", served.port)).unwrap();
    let head = "POST /repo/git-upload-pack HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n";
    stuck.write_all(head.as_bytes()).unwrap();
    // A later connection served proves that the server accepted the stuck one, which came first.
    let response = request(served.port, "/repo/info/refs?service=git-upload-pack").unwrap();
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");

    served.signal("INT");
    served.wait_for("Received SIGINT, shutting down");
    let second = Instant::now();
    served.signal("INT");
    let status = wait(&mut served.child);
    let elapsed = second.elapsed();
    served.log.extend(served.logged.try_iter());
    let log = &served.log;

    assert_eq!(status.code(), Some(1), "{log:#?}");
    assert!(
        elapsed < Duration::from_secs(4),
        "the second signal took {elapsed:?} to stop the server"
    );
    assert!(
        log.iter().any(|line| line.contains("Received SIGINT again")),
        "{log:#?}"
    );
    drop(stuck);
}
