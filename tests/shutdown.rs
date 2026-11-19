//! The graceful shutdown of the server: driven through a future in the process, and through real signals on Unix.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use gitserve::server;
use tokio::sync::oneshot;

/// A server on a router with a slow route and a route that never answers, run as `serve` runs it: in a runtime of
/// its own, dropped once the server returned.
struct Running {
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    /// Receives what the server returned, once its runtime is dropped.
    done: mpsc::Receiver<std::io::Result<()>>,
}

impl Running {
    fn start(grace: Duration) -> Self {
        let router = Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    "done"
                }),
            )
            .route(
                "/hang",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    "never"
                }),
            );

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

const WAIT: Duration = Duration::from_secs(10);

#[test]
fn a_request_in_flight_completes() {
    let mut server = Running::start(Duration::from_secs(5));
    let response = server.get("/slow");
    thread::sleep(Duration::from_millis(200));
    server.stop();

    let response = response.recv_timeout(WAIT).unwrap().unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("done"), "{response}");
    server.done.recv_timeout(WAIT).unwrap().unwrap();
}

#[test]
fn a_new_connection_is_refused_once_the_shutdown_started() {
    let mut server = Running::start(Duration::from_secs(5));
    // A request in flight keeps the server shutting down while the new connection is tried.
    let in_flight = server.get("/slow");
    thread::sleep(Duration::from_millis(200));
    server.stop();
    thread::sleep(Duration::from_millis(100));

    if let Ok(response) = request(server.port, "/slow") {
        assert!(
            response.is_empty(),
            "a connection was served after the shutdown: {response}"
        );
    }
    assert!(in_flight.recv_timeout(WAIT).unwrap().unwrap().ends_with("done"));
    server.done.recv_timeout(WAIT).unwrap().unwrap();
}

#[test]
fn a_request_outlasting_the_grace_is_cut() {
    let mut server = Running::start(Duration::from_millis(300));
    let response = server.get("/hang");
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    server.stop();

    server.done.recv_timeout(WAIT).unwrap().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the shutdown took {:?}",
        started.elapsed()
    );
    // The connection is closed without a response.
    if let Ok(response) = response.recv_timeout(WAIT).unwrap() {
        assert!(!response.contains("never"), "{response}");
    }
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

/// Stops a `serve` of an archive store with `signal`, and checks that it shuts down: it logs the signal, exits with
/// 0, and removes the image of the archive from the temporary directory.
#[cfg(unix)]
fn stop_with(signal: &str) {
    use std::io::{BufRead, BufReader};

    let dir = tempfile::tempdir().unwrap();
    let temporary = dir.path().join("tmp");
    std::fs::create_dir(&temporary).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_gitserve"))
        .args(["serve", "-p", "0", "repos.zip"])
        .current_dir(dir.path())
        .env("TMPDIR", &temporary)
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

    // Wait until the server listens, with the image of the archive extracted.
    let mut log = Vec::new();
    let port: u16 = loop {
        let line = logged.recv_timeout(WAIT).expect("the server did not start");
        let port = line
            .split("on port ")
            .nth(1)
            .and_then(|rest| rest.split(',').next()?.parse().ok());
        log.push(line);
        if let Some(port) = port {
            break port;
        }
    };
    let images = || std::fs::read_dir(&temporary).unwrap().count();
    assert_eq!(images(), 1);
    assert!(
        request(port, "/repos/info/refs?service=git-upload-pack")
            .unwrap()
            .starts_with("HTTP/1.1 404")
    );

    let pid = child.id().to_string();
    assert!(
        Command::new("kill")
            .args([&format!("-{signal}"), &pid])
            .status()
            .unwrap()
            .success()
    );
    let status = wait(&mut child);
    log.extend(logged.try_iter());

    assert_eq!(status.code(), Some(0), "{log:#?}");
    let expected = format!("Received SIG{signal}, shutting down");
    assert!(log.iter().any(|line| line.contains(&expected)), "{log:#?}");
    assert_eq!(images(), 0, "the image of the archive was left behind");
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
