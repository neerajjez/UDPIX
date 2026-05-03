use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Args;
use parking_lot::RwLock;

use udpix_controlplane::auth::AuthEngine;
use udpix_controlplane::policy::PolicyEngine;
use udpix_controlplane::server::ServerBuilder;
use udpix_controlplane::session_mgr::SessionManager;

#[derive(Args)]
pub struct ServerArgs {
    /// Address to listen on
    #[arg(long, default_value = "0.0.0.0:9000")]
    pub addr: SocketAddr,

    /// Path to TLS certificate PEM (omit for insecure/dev mode)
    #[arg(long)]
    pub cert: Option<PathBuf>,

    /// Path to TLS private key PEM
    #[arg(long)]
    pub key: Option<PathBuf>,

    /// Default bandwidth cap per session in bytes/sec
    #[arg(long, default_value = "10000000")]
    pub default_max_bps: u64,

    /// Admin username (reads UDPIX_ADMIN_USER env var as fallback)
    #[arg(long, env = "UDPIX_ADMIN_USER", default_value = "admin")]
    pub admin_user: String,

    /// Admin password (reads UDPIX_ADMIN_PASS env var as fallback)
    #[arg(long, env = "UDPIX_ADMIN_PASS", default_value = "changeme")]
    pub admin_pass: String,
}

pub async fn run(args: ServerArgs) -> anyhow::Result<()> {
    // Build auth engine with a random JWT secret
    let jwt_secret: Vec<u8> = {
        use rand::RngCore;
        let mut s = vec![0u8; 32];
        rand::thread_rng().fill_bytes(&mut s);
        s
    };
    let mut auth = AuthEngine::new(jwt_secret, 3600);
    auth.add_user(args.admin_user.clone(), &args.admin_pass)
        .context("add_user")?;
    let auth = Arc::new(auth);

    let sessions = SessionManager::new(Duration::from_secs(300));
    let policies = Arc::new(RwLock::new(PolicyEngine::new(args.default_max_bps)));

    let builder = ServerBuilder::new(auth, sessions, policies);

    match (args.cert, args.key) {
        (Some(cert_path), Some(key_path)) => {
            let cert_pem = std::fs::read(&cert_path)
                .with_context(|| format!("read cert {}", cert_path.display()))?;
            let key_pem = std::fs::read(&key_path)
                .with_context(|| format!("read key {}", key_path.display()))?;
            tracing::info!("UDPix server (TLS) listening on {}", args.addr);
            builder.serve_tls(args.addr, &cert_pem, &key_pem).await
        }
        _ => {
            tracing::warn!("TLS disabled — insecure mode; use --cert/--key for production");
            tracing::info!("UDPix server listening on {}", args.addr);
            builder.serve_insecure(args.addr).await
        }
    }
}
