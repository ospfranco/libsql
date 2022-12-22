use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};

/// ChiselEdge CLI
#[derive(Debug, Parser)]
#[command(name = "edge")]
#[command(about = "ChiselEdge CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start a ChiselEdge server.
    Serve {
        #[clap(long, short, default_value = "iku.db")]
        db_path: PathBuf,
        #[clap(long, short, default_value = "127.0.0.1:5000")]
        tcp_addr: SocketAddr,
        #[clap(long, short)]
        ws_addr: Option<SocketAddr>,
        #[clap(long, short)]
        fdb_config_path: Option<String>,
        #[clap(long, short)]
        remote_writer_url: Option<String>,
        /// run this node in write proxy mode at this address
        #[clap(long, conflicts_with = "remote_writer_url")]
        write_rpc_server_addr: Option<SocketAddr>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Cli::parse();
    // This is how foundationdb crate recommends its initialization,
    // along with dropping `network` manually before the program ends:
    // https://docs.rs/foundationdb/0.7.0/foundationdb/fn.boot.html
    #[cfg(feature = "fdb")]
    let network = unsafe { foundationdb::boot() };

    match args.command {
        Commands::Serve {
            db_path,
            tcp_addr,
            ws_addr,
            fdb_config_path,
            remote_writer_url,
            write_rpc_server_addr,
        } => {
            server::run_server(
                db_path,
                tcp_addr,
                ws_addr,
                fdb_config_path,
                remote_writer_url,
                write_rpc_server_addr,
            )
            .await?;
        }
    }

    #[cfg(feature = "fdb")]
    drop(network);
    Ok(())
}