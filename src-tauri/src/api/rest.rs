use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::BodyExt;
use tracing::info;

use crate::AppState;

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

fn full_body(data: Vec<u8>) -> BoxBody {
    http_body_util::Full::new(Bytes::from(data)).boxed()
}

#[cfg(feature = "grpc")]
pub struct RestApiServer {
    addr: SocketAddr,
    state: Arc<AppState>,
}

#[cfg(feature = "grpc")]
impl RestApiServer {
    pub fn new(addr: SocketAddr, state: Arc<AppState>) -> Self {
        Self { addr, state }
    }

    pub async fn start(self) -> Result<()> {
        info!(target: "nine_snake.rest", addr = %self.addr, "REST API server starting");
        let state = self.state;
        let addr = self.addr;

        let service = move |req: hyper::Request<hyper::body::Incoming>| {
            let state = state.clone();
            async move {
                let path = req.uri().path().to_string();
                let method = req.method().clone();

                let (status, body) = match (method.as_str(), path.as_str()) {
                    ("GET", "/api/health") => (200, serde_json::json!({"status": "ok"})),

                    ("GET", "/api/memories") => match state.sqlite.list_recent(50).await {
                        Ok(memories) => (200, serde_json::json!({"memories": memories})),
                        Err(e) => (500, serde_json::json!({"error": e.to_string()})),
                    },

                    ("GET", "/api/skills") => match state.skills.list_skills(Default::default()) {
                        Ok(skills) => (200, serde_json::json!({"skills": skills})),
                        Err(e) => (500, serde_json::json!({"error": e.to_string()})),
                    },

                    ("POST", "/api/chat") => (
                        200,
                        serde_json::json!({"message": "use Tauri IPC for chat"}),
                    ),
                    ("POST", "/api/swarm/execute") => (
                        200,
                        serde_json::json!({"message": "use Tauri IPC for swarm"}),
                    ),
                    _ => (404, serde_json::json!({"error": "not found"})),
                };

                let body_bytes = serde_json::to_vec(&body).unwrap_or_default();
                let resp = match hyper::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(full_body(body_bytes))
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to build REST response");
                        hyper::Response::builder()
                            .status(500)
                            .header("content-type", "application/json")
                            .body(full_body(Vec::new()))
                            .unwrap()
                    }
                };
                Ok::<_, Infallible>(resp)
            }
        };

        let listener = tokio::net::TcpListener::bind(addr).await?;
        info!(target: "nine_snake.rest", "REST API server listening on {}", addr);

        loop {
            let (stream, _) = listener.accept().await?;
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = service.clone();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, hyper::service::service_fn(service))
                    .await;
            });
        }
    }
}

#[cfg(not(feature = "grpc"))]
pub struct RestApiServer {
    _private: (),
}

#[cfg(not(feature = "grpc"))]
impl RestApiServer {
    pub fn new(_: SocketAddr, _: Arc<AppState>) -> Self {
        Self { _private: () }
    }

    pub async fn start(self) -> Result<()> {
        tracing::warn!("REST API server disabled (grpc feature not enabled)");
        Ok(())
    }
}
