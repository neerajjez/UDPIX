use clap::{Parser, Subcommand};

mod receive;
mod send;
mod server;

#[derive(Parser)]
#[command(name = "udpix", about = "Enterprise high-speed WAN file transfer")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the UDPix server (gRPC control plane + UDP data plane)
    Server(server::ServerArgs),
    /// Send a file or directory to a UDPix server
    Send(send::SendArgs),
    /// Receive files from a UDPix peer
    Receive(receive::ReceiveArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Server(args)  => server::run(args).await,
        Commands::Send(args)    => send::run(args).await,
        Commands::Receive(args) => receive::run(args).await,
    }
}
