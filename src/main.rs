use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gitserve::server;
use gitserve::store::Store;

/// The port that `serve` listens on by default.
const DEFAULT_PORT: u16 = 8085;

/// Serve local bare Git repositories over HTTP.
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
    /// The repository in the directory <name> of the store is served at http://<host>:<port><prefix><name>.
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
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve { port, prefix, store } => run_server(store, port, &prefix),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Serves the repositories of the directory `store` on `port`, under the URL `prefix`.
fn run_server(store: PathBuf, port: u16, prefix: &str) -> anyhow::Result<()> {
    if !store.is_dir() {
        anyhow::bail!("{} is not a directory", store.display());
    }

    let prefix = server::normalize_prefix(prefix);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        let port = listener.local_addr()?.port();
        log::info!("Serving {} on port {port}, under {prefix}", store.display());
        server::serve(listener, server::router(Store::new(store), prefix)).await?;
        Ok(())
    })
}
