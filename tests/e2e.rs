//! End-to-end tests: the real `git` client against the server.
//!
//! They need `git`, and are skipped without it, except on the CI (when the `CI` variable is set), where its absence
//! is a failure.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;

use gitserve::server;
use gitserve::store::Store;
use tempfile::TempDir;

/// A server on a store of its own, with an isolated Git configuration for the client.
struct Fixture {
    dir: TempDir,
    base_url: String,
}

impl Fixture {
    /// Starts a server, or returns `None` when `git` is missing outside of the CI.
    fn start() -> Option<Self> {
        if Command::new("git").arg("--version").output().is_err() {
            assert!(std::env::var_os("CI").is_none(), "git is required on the CI");
            eprintln!("git is missing: skipping the end-to-end test");
            return None;
        }

        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("root")).unwrap();
        fs::write(
            dir.path().join("gitconfig"),
            "[user]\n\tname = Test\n\temail = test@example.com\n\
             [init]\n\tdefaultBranch = main\n[core]\n\tautocrlf = false\n",
        )
        .unwrap();

        let root = dir.path().join("root");
        let (port_sender, port_receiver) = mpsc::channel();
        thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                port_sender.send(listener.local_addr().unwrap().port()).unwrap();
                server::serve(listener, server::router(Store::new(root), "/git/".to_string()))
                    .await
                    .unwrap();
            });
        });

        let port = port_receiver.recv().unwrap();
        Some(Self {
            dir,
            base_url: format!("http://127.0.0.1:{port}/git/"),
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn url(&self, name: &str) -> String {
        format!("{}{name}", self.base_url)
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

    let work = fx.clone("sha256", "work", 2);
    write_lines(&work, "file.txt", 100, "content");
    fx.git(&work, &["add", "."]);
    fx.git(&work, &["commit", "--quiet", "-m", "first"]);
    fx.git(&work, &["push", "--quiet", "origin", "HEAD:main"]);

    let copy = fx.clone("sha256", "copy", 0);
    assert_eq!(fx.head(&copy), fx.head(&work));
    assert_eq!(fx.head(&copy).len(), 64);
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

    for name in ["missing", "present/..", "..%2Fpresent"] {
        let url = fx.url(name);
        let output = fx.try_git(fx.dir.path(), &["ls-remote", &url]);
        assert!(!output.status.success(), "ls-remote {name} succeeded");
    }
}
