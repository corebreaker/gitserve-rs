use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gitserve::store::{self, Store};
use gitserve::{repository, server};

/// The port that `serve` listens on by default.
const DEFAULT_PORT: u16 = 8085;

/// Serve local bare Git repositories over HTTP.
///
/// gitserve serves bare Git repositories over the smart HTTP protocol of Git, so that they can be cloned and pushed
/// to without a forge, and without Git installed on the serving machine. The repositories live in a store,
/// a directory.
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
        /// Directory holding the repositories.
        store: PathBuf,
    },
    /// Create an empty store.
    ///
    /// Create an empty store, along with its missing parent directories.
    Init {
        /// Directory to create.
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
        /// Directory holding the repositories.
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
        /// Directory holding the repositories.
        store: PathBuf,
        /// Names of the repositories to delete.
        #[arg(required = true)]
        repositories: Vec<String>,
    },
    /// List the repositories of a store.
    List {
        /// Directory holding the repositories.
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

    let store = Store::open(path)?;
    let prefix = server::normalize_prefix(prefix);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        let port = listener.local_addr()?.port();
        log::info!("Serving {} on port {port}, under {prefix}", path.display());
        server::serve(listener, server::router(store, prefix)).await?;
        Ok(())
    })
}

/// Creates the repositories `names` in the store at `path`, initializing the store when needed.
fn create(path: &Path, names: &[String], branch: &str) -> anyhow::Result<()> {
    if store::ensure_store(path)? {
        println!("Initialized empty store {}", path.display());
    }

    let store = Store::open(path)?;
    apply(names, |name| {
        store.create_repository(name, branch)?;
        Ok(format!("Created repository {name}"))
    })
}

/// Deletes the repositories `names` from the store at `path`.
fn delete(path: &Path, names: &[String]) -> anyhow::Result<()> {
    let store = Store::open(path)?;
    apply(names, |name| {
        let dir = store.delete_repository(name)?;
        Ok(format!("Deleted repository {dir}"))
    })
}

/// Prints the repositories of the store at `path`.
fn list(path: &Path) -> anyhow::Result<()> {
    for name in Store::open(path)?.repositories()? {
        println!("{name}");
    }
    Ok(())
}

/// Runs `action` on each name, printing what it reports, and goes on with the other names when one fails:
/// it reports each failure, and fails once all are done.
fn apply(names: &[String], mut action: impl FnMut(&str) -> anyhow::Result<String>) -> anyhow::Result<()> {
    let mut failures = 0;
    for name in names {
        match action(name) {
            Ok(report) => println!("{report}"),
            Err(err) => {
                eprintln!("Error: {name}: {err:#}");
                failures += 1;
            }
        }
    }

    match failures {
        0 => Ok(()),
        1 => anyhow::bail!("1 repository failed"),
        _ => anyhow::bail!("{failures} repositories failed"),
    }
}
