use std::net::SocketAddr;
use std::sync::Arc;

use tonic::{Request, Response, Status};
use tonic::transport::Server;

use crate::auth::AuthEngine;
use crate::policy::PolicyEngine;
use crate::session_mgr::SessionManager;
use crate::proto::control_plane_server::{ControlPlane, ControlPlaneServer};
use crate::proto::{
    AuthRequest, AuthResponse,
    HeartbeatRequest, HeartbeatResponse,
    TerminateRequest, TerminateResponse,
};

// ── Service implementation ────────────────────────────────────────────────────

pub struct ControlService {
    auth:     Arc<AuthEngine>,
    sessions: Arc<SessionManager>,
    policies: Arc<parking_lot::RwLock<PolicyEngine>>,
}

impl ControlService {
    pub fn new(
        auth:     Arc<AuthEngine>,
        sessions: Arc<SessionManager>,
        policies: Arc<parking_lot::RwLock<PolicyEngine>>,
    ) -> Self {
        Self { auth, sessions, policies }
    }
}

#[tonic::async_trait]
impl ControlPlane for ControlService {
    async fn authenticate(
        &self,
        request: Request<AuthRequest>,
    ) -> Result<Response<AuthResponse>, Status> {
        let req = request.into_inner();

        if !self.auth.authenticate(&req.username, &req.password) {
            return Err(Status::unauthenticated("invalid credentials"));
        }

        let policy = self.policies.read().resolve(&req.username);
        let session = self
            .sessions
            .create(req.username.clone(), policy.max_bps)
            .map_err(|e| Status::internal(e.to_string()))?;

        let jwt = self
            .auth
            .issue_token(&req.username, &session.session_uuid)
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(AuthResponse {
            session_id:    session.session_uuid,
            jwt_token:     jwt,
            session_key:   session.session_key.to_vec(),
            session_nonce: session.session_nonce.to_vec(),
            max_bps:       session.max_bps,
        }))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        self.auth
            .verify_token(&req.jwt_token)
            .map_err(|_| Status::unauthenticated("invalid token"))?;

        let ok = self.sessions.heartbeat(&req.session_id);
        Ok(Response::new(HeartbeatResponse { ok }))
    }

    async fn terminate(
        &self,
        request: Request<TerminateRequest>,
    ) -> Result<Response<TerminateResponse>, Status> {
        let req = request.into_inner();
        self.auth
            .verify_token(&req.jwt_token)
            .map_err(|_| Status::unauthenticated("invalid token"))?;

        let ok = self.sessions.terminate(&req.session_id);
        Ok(Response::new(TerminateResponse { ok }))
    }
}

// ── Server builder ────────────────────────────────────────────────────────────

pub struct ServerBuilder {
    auth:     Arc<AuthEngine>,
    sessions: Arc<SessionManager>,
    policies: Arc<parking_lot::RwLock<PolicyEngine>>,
}

impl ServerBuilder {
    pub fn new(
        auth:     Arc<AuthEngine>,
        sessions: Arc<SessionManager>,
        policies: Arc<parking_lot::RwLock<PolicyEngine>>,
    ) -> Self {
        Self { auth, sessions, policies }
    }

    fn service(self) -> ControlPlaneServer<ControlService> {
        ControlPlaneServer::new(ControlService::new(self.auth, self.sessions, self.policies))
    }

    /// Serve without TLS — for unit tests and local dev.
    pub async fn serve_insecure(self, addr: SocketAddr) -> anyhow::Result<()> {
        Server::builder()
            .add_service(self.service())
            .serve(addr)
            .await
            .map_err(Into::into)
    }

    /// Serve with rustls TLS 1.3 — production path.
    pub async fn serve_tls(
        self,
        addr: SocketAddr,
        cert_pem: &[u8],
        key_pem: &[u8],
    ) -> anyhow::Result<()> {
        use tonic::transport::{Identity, ServerTlsConfig};

        let identity = Identity::from_pem(cert_pem, key_pem);
        let tls = ServerTlsConfig::new().identity(identity);

        Server::builder()
            .tls_config(tls)?
            .add_service(self.service())
            .serve(addr)
            .await
            .map_err(Into::into)
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tonic::transport::Channel;

    use crate::auth::AuthEngine;
    use crate::policy::PolicyEngine;
    use crate::session_mgr::SessionManager;
    use crate::proto::control_plane_client::ControlPlaneClient;

    fn make_stack() -> (Arc<AuthEngine>, Arc<SessionManager>, Arc<parking_lot::RwLock<PolicyEngine>>) {
        let mut auth = AuthEngine::new(b"test-secret-key-32-bytes-long!!!".to_vec(), 3600);
        auth.add_user("alice".into(), "s3cr3t").unwrap();
        let auth = Arc::new(auth);
        let sessions = SessionManager::new(Duration::from_secs(60));
        let policies = Arc::new(parking_lot::RwLock::new(PolicyEngine::new(100_000_000)));
        (auth, sessions, policies)
    }

    #[tokio::test]
    async fn grpc_authenticate_loopback() {
        let (auth, sessions, policies) = make_stack();

        // Bind on an OS-assigned port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn server
        let svc = ControlPlaneServer::new(ControlService::new(
            auth.clone(), sessions.clone(), policies.clone(),
        ));
        tokio::spawn(async move {
            Server::builder()
                .add_service(svc)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        // Give the server a moment to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        let channel = Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = ControlPlaneClient::new(channel);

        // Successful auth
        let resp = client
            .authenticate(AuthRequest {
                username: "alice".into(),
                password: "s3cr3t".into(),
            })
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.session_id.is_empty());
        assert!(!resp.jwt_token.is_empty());
        assert_eq!(resp.session_key.len(), 32);
        assert_eq!(resp.session_nonce.len(), 12);
        assert_eq!(resp.max_bps, 100_000_000);

        // Heartbeat with valid JWT
        let hb = client
            .heartbeat(HeartbeatRequest {
                session_id: resp.session_id.clone(),
                jwt_token:  resp.jwt_token.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert!(hb.ok);

        // Terminate
        let term = client
            .terminate(TerminateRequest {
                session_id: resp.session_id.clone(),
                jwt_token:  resp.jwt_token.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert!(term.ok);

        // Wrong password
        let err = client
            .authenticate(AuthRequest {
                username: "alice".into(),
                password: "wrong".into(),
            })
            .await;
        assert!(err.is_err());
    }
}
