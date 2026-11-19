//! End-to-end tests: the real `git` client against the server.
//!
//! They need `git`, and are skipped without it, except on the CI (when the `CI` variable is set), where its absence
//! is a failure.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gitserve::server;
use gitserve::store::Store;
use tempfile::TempDir;

/// A server on a store of its own, with an isolated Git configuration for the client.
struct Fixture {
    dir: TempDir,
    port: u16,
}

impl Fixture {
    /// Starts a server on the directory store `root`, or returns `None` when `git` is missing outside of the CI.
    fn start() -> Option<Self> {
        Self::start_with("root", |path| fs::create_dir(path).unwrap())
    }

    /// Starts a server on the store at `store` in the directory of the fixture, which `prepare` makes, or returns
    /// `None` when `git` is missing outside of the CI.
    fn start_with(store: &str, prepare: impl FnOnce(&Path)) -> Option<Self> {
        if Command::new("git").arg("--version").output().is_err() {
            assert!(std::env::var_os("CI").is_none(), "git is required on the CI");
            eprintln!("git is missing: skipping the end-to-end test");
            return None;
        }

        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join(store);
        if let Some(parent) = store.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        prepare(&store);
        fs::write(
            dir.path().join("gitconfig"),
            "[user]\n\tname = Test\n\temail = test@example.com\n\
             [init]\n\tdefaultBranch = main\n[core]\n\tautocrlf = false\n",
        )
        .unwrap();

        let store = Store::open(&store).unwrap();
        let (port_sender, port_receiver) = mpsc::channel();
        thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                port_sender.send(listener.local_addr().unwrap().port()).unwrap();
                let router = server::router(std::sync::Arc::new(store), "/git/".to_string());
                server::serve(listener, router, std::future::pending(), server::SHUTDOWN_GRACE)
                    .await
                    .unwrap();
            });
        });

        let port = port_receiver.recv().unwrap();
        Some(Self { dir, port })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/git/{name}", self.port)
    }

    /// Sends an HTTP request to the server, and returns the status code, the headers and the body of the response.
    fn http(&self, method: &str, path: &str, headers: &[&str], body: &[u8]) -> (u16, String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        for header in headers {
            request.push_str(&format!("{header}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        stream.write_all(request.as_bytes()).unwrap();
        stream.write_all(body).unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let split = response.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
        let head = String::from_utf8(response[..split].to_vec()).unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, head, response[split + 4..].to_vec())
    }

    /// Runs `git` in `dir` with `args`, and returns its output whether it succeeds or not.
    fn try_git(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.path("gitconfig"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap()
    }

    /// Runs `git` in `dir` with `args`, checks that it succeeds, and returns its standard output, trimmed.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.try_git(dir, args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// Runs the gitserve binary in the directory of the fixture with `args` (see [`run_gitserve`]).
    fn gitserve(&self, args: &[&str]) -> (i32, String, String) {
        run_gitserve(self.dir.path(), args)
    }

    /// Creates the bare repository `name` in the store, with extra `git init` arguments.
    fn create_repository(&self, name: &str, args: &[&str]) {
        let mut init = vec!["init", "--bare", "--quiet"];
        init.extend_from_slice(args);
        init.push(name);
        self.git(&self.path("root"), &init);
    }

    /// Clones the repository served under `name` into `dir`, with a protocol version.
    fn clone(&self, name: &str, dir: &str, version: u8) -> PathBuf {
        let protocol = format!("protocol.version={version}");
        let url = self.url(name);
        self.git(self.dir.path(), &["-c", &protocol, "clone", "--quiet", &url, dir]);
        self.path(dir)
    }

    fn head(&self, dir: &Path) -> String {
        self.git(dir, &["rev-parse", "HEAD"])
    }

    /// Checks the served repository `dir` (a directory of the store) with `git fsck`.
    fn fsck(&self, dir: &str) {
        self.git(
            &self.path("root").join(dir),
            &["fsck", "--strict", "--full", "--no-progress"],
        );
    }
}

/// Runs the gitserve binary in `dir` with `args`, and returns its exit code and its standard output and error.
fn run_gitserve(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_gitserve"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap();
    (output.status.code().unwrap(), text(output.stdout), text(output.stderr))
}

/// Returns a directory holding an empty store `root`, for the tests that only run the binary: they need neither
/// `git` nor a server, so they always run.
fn workspace() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("root")).unwrap();
    dir
}

/// Writes `lines` numbered lines tagged with `tag` to `dir/name`.
fn write_lines(dir: &Path, name: &str, lines: usize, tag: &str) {
    let content: String = (1..=lines).map(|i| format!("{i} line of {name} {tag}\n")).collect();
    fs::write(dir.join(name), content).unwrap();
}

#[test]
fn clone_push_and_fetch_in_both_protocol_versions() {
    let Some(fx) = Fixture::start() else { return };
    fx.create_repository("demo.git", &[]);

    // An empty repository, served under its name without the .git suffix.
    let work = fx.clone("demo", "work", 2);
    for name in ["a.txt", "b.txt", "c.txt"] {
        write_lines(&work, name, 2000, "first");
    }
    fx.git(&work, &["add", "."]);
    fx.git(&work, &["commit", "--quiet", "-m", "first"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);

    // The second push sends a thin pack, whose bases are on the server only.
    write_lines(&work, "a.txt", 2000, "second");
    fx.git(&work, &["commit", "--quiet", "-am", "second"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);

    fx.git(&work, &["tag", "-a", "v1", "-m", "v1"]);
    fx.git(&work, &["push", "--quiet", "origin", "v1", "HEAD:topic"]);
    fx.git(&work, &["push", "--quiet", "origin", ":topic"]);

    let mut refs: Vec<String> = fx
        .git(&work, &["ls-remote", "origin"])
        .lines()
        .filter_map(|line| line.split('\t').nth(1).map(str::to_string))
        .collect();
    refs.sort();
    assert_eq!(refs, ["HEAD", "refs/heads/main", "refs/tags/v1", "refs/tags/v1^{}"]);

    let v0 = fx.clone("demo.git", "v0", 0);
    let v2 = fx.clone("demo", "v2", 2);
    assert_eq!(fx.head(&v0), fx.head(&work));
    assert_eq!(fx.head(&v2), fx.head(&work));

    // Incremental fetches, which negotiate with the objects that the clones already have.
    write_lines(&work, "b.txt", 2000, "third");
    fx.git(&work, &["commit", "--quiet", "-am", "third"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
    fx.git(&v0, &["-c", "protocol.version=0", "pull", "--quiet", "--ff-only"]);
    fx.git(&v2, &["-c", "protocol.version=2", "pull", "--quiet", "--ff-only"]);
    assert_eq!(fx.head(&v0), fx.head(&work));
    assert_eq!(fx.head(&v2), fx.head(&work));

    // A forced push rewrites the history of the server.
    fx.git(&work, &["reset", "--quiet", "--hard", "HEAD~1"]);
    fx.git(&work, &["commit", "--quiet", "--allow-empty", "-m", "rewritten"]);
    let rejected = fx.try_git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
    assert!(!rejected.status.success(), "a non-fast-forward push was accepted");
    fx.git(&work, &["push", "--quiet", "--force", "origin", "HEAD:main"]);
    assert_eq!(
        fx.git(&v2, &["ls-remote", "origin", "refs/heads/main"])
            .split('\t')
            .next(),
        Some(fx.head(&work).as_str())
    );

    fx.fsck("demo.git");
}

#[test]
fn a_sha256_repository_is_served() {
    let Some(fx) = Fixture::start() else { return };
    fx.create_repository("sha256", &["--object-format=sha256"]);

    // The work repository is not cloned from the empty one: before Git 2.42, such a clone was made with SHA-1.
    let work = fx.path("work");
    fx.git(fx.dir.path(), &["init", "--quiet", "--object-format=sha256", "work"]);
    fx.git(&work, &["remote", "add", "origin", &fx.url("sha256")]);
    write_lines(&work, "file.txt", 100, "content");
    fx.git(&work, &["add", "."]);
    fx.git(&work, &["commit", "--quiet", "-m", "first"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);

    for version in [0, 2] {
        let copy = fx.clone("sha256", &format!("copy-v{version}"), version);
        assert_eq!(fx.head(&copy), fx.head(&work));
        assert_eq!(fx.head(&copy).len(), 64);
    }
    fx.fsck("sha256");
}

#[test]
fn a_large_push_is_accepted() {
    let Some(fx) = Fixture::start() else { return };
    fx.create_repository("large.git", &[]);

    // Bytes that do not compress, so that the pack is as large as the file.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let content: Vec<u8> = (0..8 << 20)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();

    let work = fx.clone("large", "work", 2);
    fs::write(work.join("random.bin"), &content).unwrap();
    fx.git(&work, &["add", "."]);
    fx.git(&work, &["commit", "--quiet", "-m", "large"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);

    let copy = fx.clone("large", "copy", 2);
    assert_eq!(fs::read(copy.join("random.bin")).unwrap(), content);
    fx.fsck("large.git");
}

#[test]
fn a_missing_repository_is_not_found() {
    let Some(fx) = Fixture::start() else { return };
    fx.create_repository("present.git", &[]);

    let advertisement = "info/refs?service=git-upload-pack";
    assert_eq!(
        fx.http("GET", &format!("/git/present/{advertisement}"), &[], b"").0,
        200
    );
    for name in ["missing", "present/..", "..%2Fpresent", "%2E%2E", "present%5C"] {
        let path = format!("/git/{name}/{advertisement}");
        assert_eq!(fx.http("GET", &path, &[], b"").0, 404, "{path}");
    }
    assert_eq!(
        fx.http("GET", &format!("/other/present/{advertisement}"), &[], b"").0,
        404
    );

    let output = fx.try_git(fx.dir.path(), &["ls-remote", &fx.url("missing")]);
    assert!(!output.status.success(), "ls-remote of a missing repository succeeded");
}

#[test]
fn the_dumb_protocol_and_wrong_methods_are_refused() {
    let Some(fx) = Fixture::start() else { return };
    fx.create_repository("present.git", &[]);

    assert_eq!(fx.http("GET", "/git/present/info/refs", &[], b"").0, 403);
    assert_eq!(fx.http("GET", "/git/present/objects/info/packs", &[], b"").0, 404);

    let (status, head, _) = fx.http("GET", "/git/present/git-upload-pack", &[], b"");
    assert_eq!(status, 405);
    assert!(head.to_ascii_lowercase().contains("allow: post"), "{head}");

    let (status, head, _) = fx.http("POST", "/git/present/info/refs?service=git-upload-pack", &[], b"");
    assert_eq!(status, 405);
    assert!(head.to_ascii_lowercase().contains("allow: get"), "{head}");
}

#[test]
fn a_truncated_request_is_refused() {
    let Some(fx) = Fixture::start() else { return };
    fx.create_repository("demo.git", &[]);

    // The body announces more bytes than the client sends before closing its side of the connection.
    let mut stream = TcpStream::connect(("127.0.0.1", fx.port)).unwrap();
    let head = "POST /git/demo/git-upload-pack HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n";
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(b"0000").unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    // A server that kept the connection open would fail the test rather than hang it.
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
}

/// Returns `line` as a packet line.
fn pkt_line(line: &str) -> String {
    format!("{:04x}{line}", line.len() + 4)
}

#[test]
fn a_gzipped_request_is_decoded() {
    let Some(fx) = Fixture::start() else { return };
    fx.create_repository("demo.git", &[]);
    let work = fx.clone("demo", "work", 2);
    write_lines(&work, "file.txt", 10, "content");
    fx.git(&work, &["add", "."]);
    fx.git(&work, &["commit", "--quiet", "-m", "first"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);

    let request = format!(
        "{}{}0001{}0000",
        pkt_line("command=ls-refs\n"),
        pkt_line("object-format=sha1\n"),
        pkt_line("ref-prefix refs/heads/\n")
    );
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(request.as_bytes()).unwrap();
    let gzipped = encoder.finish().unwrap();
    let headers = ["Git-Protocol: version=2", "Content-Encoding: gzip"];

    let (status, _, body) = fx.http("POST", "/git/demo/git-upload-pack", &headers, &gzipped);
    assert_eq!(status, 200);
    let expected = format!("{} refs/heads/main", fx.head(&work));
    assert!(
        String::from_utf8_lossy(&body).contains(&expected),
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (status, _, _) = fx.http("POST", "/git/demo/git-upload-pack", &headers, b"not gzip");
    assert_eq!(status, 400);
}

#[test]
fn names_that_need_encoding_are_served() {
    let Some(fx) = Fixture::start() else { return };
    fx.create_repository("my repo.git", &[]);
    fx.create_repository("été", &[]);

    for (name, url_name) in [("my repo.git", "my%20repo"), ("été", "%C3%A9t%C3%A9")] {
        let work = fx.clone(url_name, "work", 2);
        write_lines(&work, "file.txt", 10, name);
        fx.git(&work, &["add", "."]);
        fx.git(&work, &["commit", "--quiet", "-m", "first"]);
        fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);

        let copy = fx.clone(url_name, "copy", 0);
        assert_eq!(fx.head(&copy), fx.head(&work), "{name}");
        fx.fsck(name);
        fs::remove_dir_all(&work).unwrap();
        fs::remove_dir_all(&copy).unwrap();
    }
}

#[test]
fn commands_create_list_and_delete_repositories() {
    let Some(fx) = Fixture::start() else { return };

    let (code, out, _) = fx.gitserve(&["create", "--branch", "master", "root", "team/tools", "plain.git"]);
    assert_eq!(code, 0);
    assert_eq!(out, "Created repository team/tools\nCreated repository plain.git\n");
    assert_eq!(
        fx.gitserve(&["list", "root"]),
        (0, "plain.git\nteam/tools\n".to_string(), String::new())
    );

    // A created repository is served, with HEAD on the branch given to create.
    let work = fx.clone("team/tools", "work", 2);
    write_lines(&work, "file.txt", 10, "content");
    fx.git(&work, &["add", "."]);
    fx.git(&work, &["commit", "--quiet", "-m", "first"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:master"]);
    let copy = fx.clone("team/tools", "copy", 0);
    assert_eq!(fx.head(&copy), fx.head(&work));
    assert_eq!(fx.git(&copy, &["rev-parse", "--abbrev-ref", "HEAD"]), "master");
    fx.fsck("team/tools");

    let (code, out, _) = fx.gitserve(&["delete", "root", "plain"]);
    assert_eq!((code, out.as_str()), (0, "Deleted repository plain.git\n"));
    assert!(!fx.path("root/plain.git").exists());

    // A failure does not stop the other names.
    let (code, out, err) = fx.gitserve(&["delete", "root", "missing", "team/tools"]);
    assert_eq!(code, 1);
    assert_eq!(out, "Deleted repository team/tools\n");
    assert!(err.contains("missing"), "{err}");
    assert_eq!(fx.gitserve(&["list", "root"]), (0, String::new(), String::new()));
}

#[test]
fn create_goes_on_after_a_failure() {
    let dir = workspace();

    let (code, out, err) = run_gitserve(dir.path(), &["create", "root", "dup", "dup", "other"]);
    assert_eq!(code, 1);
    assert_eq!(out, "Created repository dup\nCreated repository other\n");
    assert!(err.contains("repository dup already exists"), "{err}");
    assert!(err.contains("1 repository failed"), "{err}");

    let (code, out, err) = run_gitserve(dir.path(), &["create", "root", "dup", "other"]);
    assert_eq!(code, 1);
    assert_eq!(out, "");
    assert!(err.contains("2 repositories failed"), "{err}");
}

#[test]
fn commands_initialize_stores() {
    let dir = workspace();

    let (code, out, _) = run_gitserve(dir.path(), &["init", "stores/first"]);
    assert_eq!((code, out.as_str()), (0, "Initialized empty store stores/first\n"));
    assert!(dir.path().join("stores/first").is_dir());
    let (code, _, err) = run_gitserve(dir.path(), &["init", "stores/first"]);
    assert_eq!(code, 1);
    assert!(err.contains("already exists"), "{err}");

    let (code, out, _) = run_gitserve(dir.path(), &["create", "stores/second", "repo"]);
    assert_eq!(code, 0);
    assert_eq!(out, "Initialized empty store stores/second\nCreated repository repo\n");

    let (code, _, err) = run_gitserve(dir.path(), &["list", "stores/missing"]);
    assert_eq!(code, 1);
    assert!(err.contains("does not exist"), "{err}");
    let (code, out, _) = run_gitserve(dir.path(), &["init", "repos.zip"]);
    assert_eq!((code, out.as_str()), (0, "Initialized empty store repos.zip\n"));
    assert!(dir.path().join("repos.zip").is_file());
}

#[test]
fn usage_errors_exit_with_2() {
    let dir = workspace();

    for args in [
        &["create", "--branch", "a..b", "root", "repo"][..],
        &["create", "--branch=-x", "root", "repo"],
        &["create", "root"],
        &["delete", "root"],
        &["list"],
        &["serve", "--port", "70000", "root"],
        &["unknown"],
    ] {
        assert_eq!(run_gitserve(dir.path(), args).0, 2, "{args:?}");
    }
    assert_eq!(run_gitserve(dir.path(), &["list", "root"]).1, "");
}

/// Starts a server on the archive store `store/repos.zip` holding the repositories `names`, created by the binary.
fn start_archive(names: &[&str]) -> Option<Fixture> {
    let fx = Fixture::start_with("store/repos.zip", |path| gitserve::store::init_store(path).unwrap())?;
    if !names.is_empty() {
        let mut args = vec!["create", "store/repos.zip"];
        args.extend_from_slice(names);
        assert_eq!(fx.gitserve(&args).0, 0);
    }
    Some(fx)
}

/// Runs `check` on the directory of the repository `name` of the archive at `path`, as read from its file.
fn in_archive<T>(path: &Path, name: &str, check: impl FnOnce(&Path) -> T) -> T {
    Store::open(path).unwrap().view(name, check).unwrap()
}

#[test]
fn an_archive_store_is_served_and_written_back() {
    // The repository is created by another process while the server runs: the server reloads the archive.
    let Some(fx) = start_archive(&["demo.git"]) else { return };
    let archive = fx.path("store/repos.zip");

    let work = fx.clone("demo", "work", 2);
    write_lines(&work, "file.txt", 2000, "first");
    fx.git(&work, &["add", "."]);
    fx.git(&work, &["commit", "--quiet", "-m", "first"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
    write_lines(&work, "file.txt", 2000, "second");
    fx.git(&work, &["commit", "--quiet", "-am", "second"]);
    fx.git(&work, &["tag", "-a", "v1", "-m", "v1"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main", "v1"]);

    // The archive file holds the pushes, which stock git reads.
    let head = fx.head(&work);
    let stored = in_archive(&archive, "demo", |dir| {
        let git_dir = dir.to_string_lossy().into_owned();
        fx.git(
            fx.dir.path(),
            &["--git-dir", &git_dir, "fsck", "--strict", "--full", "--no-progress"],
        );
        fx.git(
            fx.dir.path(),
            &["--git-dir", &git_dir, "rev-parse", "refs/heads/main", "refs/tags/v1^{}"],
        )
    });
    assert_eq!(stored, format!("{head}\n{head}"));

    let copy = fx.clone("demo.git", "copy", 0);
    assert_eq!(fx.head(&copy), head);
    assert_eq!(fx.gitserve(&["list", "store/repos.zip"]).1, "demo.git\n");

    // A deletion by another process reaches the server too.
    assert_eq!(fx.gitserve(&["delete", "store/repos.zip", "demo"]).0, 0);
    let output = fx.try_git(fx.dir.path(), &["ls-remote", &fx.url("demo")]);
    assert!(!output.status.success(), "a deleted repository is still served");
}

#[cfg(unix)]
#[test]
fn a_push_that_cannot_be_saved_fails() {
    use std::os::unix::fs::PermissionsExt;

    let Some(fx) = start_archive(&["demo"]) else { return };
    let work = fx.clone("demo", "work", 2);
    write_lines(&work, "file.txt", 10, "first");
    fx.git(&work, &["add", "."]);
    fx.git(&work, &["commit", "--quiet", "-m", "first"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);
    let saved = fx.head(&work);

    write_lines(&work, "file.txt", 10, "lost");
    fx.git(&work, &["commit", "--quiet", "-am", "lost"]);
    // The archive cannot be replaced while its directory is read-only, unless the tests run as root.
    let store = fx.path("store");
    fs::set_permissions(&store, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(store.join("probe"), "").is_ok() {
        fs::set_permissions(&store, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("a read-only directory is still writable (root?): skipping the test");
        return;
    }
    let pushed = fx.try_git(&work, &["push", "origin", "HEAD:main"]);
    fs::set_permissions(&store, fs::Permissions::from_mode(0o755)).unwrap();

    let stderr = String::from_utf8_lossy(&pushed.stderr);
    assert!(
        !pushed.status.success(),
        "a push that was not saved succeeded: {stderr}"
    );
    assert!(stderr.contains("could not be saved"), "{stderr}");
    // The push is discarded: the server serves the archive file as it is.
    let served = fx.git(&work, &["ls-remote", "origin", "refs/heads/main"]);
    assert_eq!(served.split('\t').next(), Some(saved.as_str()));
}

/// The commit at the head of the `demo` repository of `tests/fixtures/go-store.zip`, an archive made by the Go
/// project: `gitserve create go-store.zip demo team/tools.git`, then a push of that commit and of an annotated tag
/// `v1` to `demo`.
const GO_FIXTURE_HEAD: &str = "262eda9e732ed80f1879d2452be3c75030e4b18e";

#[test]
fn an_archive_of_the_go_project_is_served() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go-store.zip");
    let Some(fx) = Fixture::start_with("go-store.zip", |path| {
        fs::copy(&fixture, path).unwrap();
    }) else {
        return;
    };
    let archive = fx.path("go-store.zip");

    assert_eq!(fx.gitserve(&["list", "go-store.zip"]).1, "demo\nteam/tools.git\n");
    let work = fx.clone("demo", "work", 2);
    assert_eq!(fx.head(&work), GO_FIXTURE_HEAD);
    assert_eq!(fx.git(&work, &["describe", "--tags"]), "v1");

    write_lines(&work, "file.txt", 10, "rust");
    fx.git(&work, &["add", "."]);
    fx.git(&work, &["commit", "--quiet", "-m", "Pushed to gitserve-rs"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);

    // The archive keeps the format of the Go project, and holds both pushes.
    let mut zip = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
    let mut marker = String::new();
    std::io::Read::read_to_string(&mut zip.by_name(".gitserve").unwrap(), &mut marker).unwrap();
    assert_eq!(marker, "{\"version\":1}\n");
    let head = fx.head(&work);
    let stored = in_archive(&archive, "demo", |dir| {
        let git_dir = dir.to_string_lossy().into_owned();
        fx.git(
            fx.dir.path(),
            &["--git-dir", &git_dir, "fsck", "--strict", "--full", "--no-progress"],
        );
        fx.git(
            fx.dir.path(),
            &["--git-dir", &git_dir, "rev-list", "--count", "refs/heads/main"],
        )
    });
    assert_eq!(stored, "2");
    assert_eq!(fx.git(&work, &["rev-parse", "HEAD~1"]), GO_FIXTURE_HEAD);
    assert_ne!(head, GO_FIXTURE_HEAD);
}
