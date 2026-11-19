use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, mpsc};
use std::thread;

use anyhow::Context;
use clap::{Parser, Subcommand};
use gitserve::store::{self, Applied, Store};
use gitserve::{repository, server};
use tokio::sync::oneshot;

/// The port that `serve` listens on by default.
const DEFAULT_PORT: u16 = 8085;

/// Serve local bare Git repositories over HTTP.
///
/// gitserve serves bare Git repositories over the smart HTTP protocol of Git, so that they can be cloned and pushed
/// to without a forge, and without Git installed on the serving machine. The repositories live in a store:
/// a directory, or a zip archive when its name ends with .zip. An archive is written back to its file after each
/// change.
#[derive(Debug, Parser)]
#[command(name = "gitserve", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the repositories of a store over HTTP.
    ///
    /// Serve the repositories of a store over HTTP on all network interfaces, until interrupted.
    /// A store that does not exist yet is initialized first. The repository in the directory <name> of the store
    /// is served at http://<host>:<port><prefix><name>. When <name> ends with .git, the repository is also served
    /// without that suffix.
    Serve {
        /// Port to listen on.
        #[arg(short, long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// URL path under which the repositories are served.
        #[arg(long, default_value = "/")]
        prefix: String,
        /// Directory or zip archive holding the repositories.
        store: PathBuf,
    },
    /// Create an empty store.
    ///
    /// Create an empty store, along with its missing parent directories: a directory, or a zip archive when the
    /// name ends with .zip. An archive holds the repositories at its root, as a directory does, next to a .gitserve
    /// file that records the version of the archive format.
    Init {
        /// Directory or zip archive to create.
        store: PathBuf,
    },
    /// Create bare repositories.
    ///
    /// Create empty bare repositories in a store. A store that does not exist yet is initialized first.
    /// A repository name may contain slashes, to create the repository in a subdirectory of the store.
    Create {
        /// Branch that HEAD points to in the new repositories.
        #[arg(short, long, default_value = "main", value_parser = parse_branch)]
        branch: String,
        /// Directory or zip archive holding the repositories.
        store: PathBuf,
        /// Names of the repositories to create.
        #[arg(required = true)]
        repositories: Vec<String>,
    },
    /// Delete repositories.
    ///
    /// Delete repositories from a store, with all their content. As when serving, a name also designates the
    /// repository that bears it followed by .git.
    Delete {
        /// Directory or zip archive holding the repositories.
        store: PathBuf,
        /// Names of the repositories to delete.
        #[arg(required = true)]
        repositories: Vec<String>,
    },
    /// List the repositories of a store.
    List {
        /// Directory or zip archive holding the repositories.
        store: PathBuf,
    },
}

/// Checks a branch name given on the command line.
fn parse_branch(branch: &str) -> Result<String, String> {
    repository::check_branch(branch)
        .map(|()| branch.to_string())
        .map_err(|err| err.to_string())
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve { port, prefix, store } => run_server(&store, port, &prefix),
        Command::Init { store } => store::init_store(&store).map(|()| {
            println!("Initialized empty store {}", store.display());
        }),
        Command::Create {
            branch,
            store,
            repositories,
        } => create(&store, &repositories, &branch),
        Command::Delete { store, repositories } => delete(&store, &repositories),
        Command::List { store } => list(&store),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Serves the repositories of the store at `path` on `port`, under the URL `prefix`.
fn run_server(path: &Path, port: u16, prefix: &str) -> anyhow::Result<()> {
    if store::ensure_store(path)? {
        log::info!("Initialized empty store {}", path.display());
    }

    let store = Arc::new(Store::open(path)?);
    let prefix = server::normalize_prefix(prefix);
    let first_signal = watch_signals()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let served = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        let port = listener.local_addr()?.port();
        log::info!("Serving {} on port {port}, under {prefix}", path.display());

        let shutdown = async move {
            match first_signal.await {
                Ok(signal) => log::info!("Received {signal}, shutting down"),
                // The watcher of the signals is gone: no signal can stop the server any more.
                Err(_) => std::future::pending().await,
            }
        };
        let router = server::router(Arc::clone(&store), prefix);
        server::serve(listener, router, shutdown, server::SHUTDOWN_GRACE).await?;
        anyhow::Ok(())
    });

    // Let a change in progress, such as an archive being written back, complete, and refuse the later ones. Dropping
    // the runtime then closes the connections left open, once its blocking tasks completed; the store is dropped last,
    // which removes the image of an archive.
    store.close();
    drop(runtime);
    drop(store);
    served
}

/// Watches the signals asking the server to stop, in a thread of its own with a runtime of its own, which live as
/// long as the process. The first signal completes the returned receiver, which starts the shutdown; a second one
/// exits at once with 1, whatever the shutdown waits for, such as a push that hangs: an archive being written is then
/// left intact, since it is written to a temporary file first, but its image stays in the temporary directory.
///
/// It returns once the handlers are installed, so that no signal is missed while the server starts.
fn watch_signals() -> anyhow::Result<oneshot::Receiver<&'static str>> {
    let (first, first_received) = oneshot::channel();
    let (installed, installation) = mpsc::channel();
    thread::Builder::new().name("signals".to_string()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(err) => return drop(installed.send(Err(err))),
        };

        runtime.block_on(async move {
            let mut signals = match Signals::install() {
                Ok(signals) => signals,
                Err(err) => return drop(installed.send(Err(err))),
            };
            let _ = installed.send(Ok(()));

            let _ = first.send(signals.next().await);
            let signal = signals.next().await;
            log::warn!("Received {signal} again: exiting without waiting for the end of the shutdown");
            std::process::exit(1);
        });
    })?;

    installation
        .recv()
        .context("the watcher of the signals ended")?
        .context("cannot install the signal handlers")?;
    Ok(first_received)
}

/// The signals asking the server to stop: SIGINT (Ctrl+C) and SIGTERM on Unix, Ctrl+C and the closing of the console
/// on Windows.
struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    interrupt: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    close: tokio::signal::windows::CtrlClose,
}

impl Signals {
    /// Installs the handlers of the signals, in the runtime that runs the caller.
    #[cfg(unix)]
    fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Installs the handlers of the signals, in the runtime that runs the caller.
    #[cfg(windows)]
    fn install() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::windows::ctrl_c()?,
            close: tokio::signal::windows::ctrl_close()?,
        })
    }

    /// Waits for the next signal, and returns its name.
    #[cfg(unix)]
    async fn next(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }

    /// Waits for the next signal, and returns its name.
    #[cfg(windows)]
    async fn next(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "Ctrl+C",
            _ = self.close.recv() => "the closing of the console",
        }
    }
}

/// Creates the repositories `names` in the store at `path`, initializing the store when needed.
fn create(path: &Path, names: &[String], branch: &str) -> anyhow::Result<()> {
    if store::ensure_store(path)? {
        println!("Initialized empty store {}", path.display());
    }

    report(Store::open(path)?.create_repositories(names, branch))
}

/// Deletes the repositories `names` from the store at `path`.
fn delete(path: &Path, names: &[String]) -> anyhow::Result<()> {
    report(Store::open(path)?.delete_repositories(names))
}

/// Prints the repositories of the store at `path`.
fn list(path: &Path) -> anyhow::Result<()> {
    for name in Store::open(path)?.repositories()? {
        println!("{name}");
    }
    Ok(())
}

/// Prints what an operation on several repositories did, then each of its failures, and fails if any.
fn report(applied: Applied) -> anyhow::Result<()> {
    for report in &applied.done {
        println!("{report}");
    }
    for err in &applied.errors {
        eprintln!("Error: {err:#}");
    }

    match applied.errors.len() {
        0 => Ok(()),
        1 => anyhow::bail!("1 repository failed"),
        failures => anyhow::bail!("{failures} repositories failed"),
    }
}
