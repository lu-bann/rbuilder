use std::{
    io,
    net::SocketAddr,
    path::PathBuf,
    process::{Child, Command},
    time::Duration,
};

use axum::routing::get;
use futures::StreamExt;
use tokio::sync::broadcast;
use tracing::{error, info};

use crate::primitives::constraints::SignedConstraints;

#[derive(Debug)]
pub enum FakeMevBoostRelayError {
    SpawnError,
    BinaryNotFound,
}

/// Helper struct to run a fake relay for testing.
/// It mainly runs a process and not much more.
/// Usage:
/// - FakeMevBoostRelay::new().spawn();
/// - Auto kill the child process when the returned FakeMevBoostRelayInstance gets dropped.
pub struct FakeMevBoostRelay {
    path: Option<PathBuf>,
}

impl Default for FakeMevBoostRelay {
    fn default() -> Self {
        Self::new()
    }
}

fn is_enabled() -> bool {
    match std::env::var("RUN_TEST_FAKE_MEV_BOOST_RELAY") {
        Ok(value) => value.to_lowercase() == "1",
        Err(_) => false,
    }
}

impl FakeMevBoostRelay {
    pub fn new() -> Self {
        Self { path: None }
    }

    pub fn spawn(self) -> Option<FakeMevBoostRelayInstance> {
        self.try_spawn().unwrap()
    }

    fn try_spawn(self) -> Result<Option<FakeMevBoostRelayInstance>, FakeMevBoostRelayError> {
        let mut cmd = if let Some(ref prg) = self.path {
            Command::new(prg)
        } else {
            Command::new("mev-boost-fake-relay")
        };
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());

        match cmd.spawn() {
            Ok(child) => Ok(Some(FakeMevBoostRelayInstance { child })),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => {
                    if is_enabled() {
                        // If the binary is not found but it is required, we should return an error
                        Err(FakeMevBoostRelayError::BinaryNotFound)
                    } else {
                        Ok(None)
                    }
                }
                _ => Err(FakeMevBoostRelayError::SpawnError),
            },
        }
    }
}

#[derive(Debug)]
pub struct FakeMevBoostRelayInstance {
    child: Child,
}

impl FakeMevBoostRelayInstance {
    pub fn endpoint(&self) -> String {
        "http://localhost:8080".to_string()
    }
}

impl Drop for FakeMevBoostRelayInstance {
    fn drop(&mut self) {
        self.child.kill().expect("could not kill mev-boost-server");
    }
}

#[derive(Clone, Debug)]
pub struct PreconfRelay {
    constraints_handle: ConstraintsHandle,
    addr: SocketAddr,
}

impl PreconfRelay {
    pub fn new(addr: SocketAddr) -> Self {
        let constraints_tx = broadcast::channel(128).0;
        let constraints_handle = ConstraintsHandle { constraints_tx };
        Self {
            constraints_handle,
            addr,
        }
    }

    pub fn spawn(self) {
        let router = axum::Router::new()
            .route(
                "/relay/v1/builder/constraints_stream",
                get(constraints_stream),
            )
            .route("/health", get(health_check))
            .with_state(self.clone());

        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind(self.addr).await.unwrap();
            axum::serve(listener, router).await.unwrap();
        });

        info!("Server running on {}", self.endpoint());
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }
}

// Health check endpoint
pub async fn health_check() -> impl axum::response::IntoResponse {
    axum::Json(serde_json::json!({"status": "OK"}))
}

pub async fn constraints_stream(
    axum::extract::Extension(preconf_relay): axum::extract::Extension<PreconfRelay>,
) -> axum::response::Sse<impl futures::Stream<Item = eyre::Result<axum::response::sse::Event>>> {
    let constraints_handle = preconf_relay.constraints_handle;
    let constraints_rx = constraints_handle.constraints_tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(constraints_rx);

    let filtered = stream.map(|result| match result {
        Ok(constraint) => match serde_json::to_string(&vec![constraint]) {
            Ok(json) => Ok(axum::response::sse::Event::default()
                .data(json)
                .event("signed_constraint")
                .retry(Duration::from_millis(50))),
            Err(err) => Err(eyre::eyre!("Error serializing constraint: {:?}", err)),
        },
        Err(err) => Err(eyre::eyre!("Error receiving constraint: {:?}", err)),
    });

    axum::response::Sse::new(filtered).keep_alive(axum::response::sse::KeepAlive::default())
}

#[derive(Clone, Debug)]
pub struct ConstraintsHandle {
    pub(crate) constraints_tx: broadcast::Sender<SignedConstraints>,
}

impl ConstraintsHandle {
    pub fn send_constraints(&self, constraints: SignedConstraints) {
        match self.constraints_tx.send(constraints) {
            Ok(res) => info!("Sent constraint: {:?}", res),
            Err(_) => error!("Failed to send constraint"),
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        net::{IpAddr, Ipv4Addr},
        sync::{Arc, Mutex},
    };

    use crate::{
        live_builder::constraint_client::ConstraintSubscriber,
        primitives::mev_boost::{MevBoostRelay, RelayConfig},
    };

    use tokio_util::sync::CancellationToken;

    use super::*;

    #[ignore]
    #[test]
    fn test_spawn_fake_mev_boost_server() {
        let srv = FakeMevBoostRelay::new().spawn();
        let _ = match srv {
            Some(srv) => srv,
            None => {
                println!("mev-boost binary not found, skipping test");
                return;
            }
        };
    }

    #[ignore]
    #[tokio::test]
    async fn test_constraint_subscriber() {
        tracing_subscriber::fmt::init();

        let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 5656);
        let relay = PreconfRelay::new(socket);
        relay.clone().spawn();

        let endpoint = relay.endpoint();
        // let endpoint = "https://holesky-preconf.titanrelay.xyz/";

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // health check
        let health_check = reqwest::get(&format!("{}/health", endpoint)).await.unwrap();
        info!("Health check response: {:?}", health_check.status());

        let config = RelayConfig::default().with_url(&endpoint);
        let relays = vec![MevBoostRelay::from_config(&config).unwrap()];
        let constraint_subscriber = ConstraintSubscriber::new(relays, CancellationToken::new());

        let constraints_handle = relay.constraints_handle;

        // Prepare multiple signed constraints
        let test_constraints: Vec<SignedConstraints> =
            serde_json::from_str(get_signed_constraints_json()).unwrap();

        let mut constraint_stream_channel = constraint_subscriber.spawn();
        // Shared vector to collect received constraints
        let received_constraints = Arc::new(Mutex::new(Vec::new()));
        let received_constraints_clone = Arc::clone(&received_constraints);

        tokio::spawn({
            async move {
                while let Some(constraint) = constraint_stream_channel.recv().await {
                    received_constraints_clone.lock().unwrap().push(constraint);
                }
            }
        });

        // Send the signed constraints
        for constraint in &test_constraints {
            constraints_handle.send_constraints(constraint.clone());
        }

        // Wait for the constraints to be received
        tokio::time::sleep(Duration::from_secs(1)).await;

        let received_constraints = received_constraints.lock().unwrap();
        assert_eq!(*received_constraints, test_constraints);
    }

    fn get_signed_constraints_json() -> &'static str {
        r#"[
            {
                "message": {
                "pubkey": "0xa20322c78fb784ba5e0d9d67ccf71e96c7efa0ea49fda73d62e58f70aab2703b0edc3ea8547c655021858f98437ee790",
                "slot": 987432,
                "top": false,
                "transactions": [ 
                    "0x02f876018204db8405f5e100850218711a00825208949d22816f6611cfcb0cde5076c5f4e4a269e79bef8904563918244f40000080c080a0ee840d80915c9b506537909a5a6cf1ca2c5b47140d6585adab6ec0faf75fdcb7a07692785c5cb43c7cf02b800f"
                ]
                },
                "signature": "0x1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505cc411d61252fb6cb3fa0017b679f8bb2305b26a285fa2737f175668d0dff91cc1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505"
            }, {
                "message": {
                "pubkey": "0xa20322c78fb784ba5e0d9d67ccf71e96c7efa0ea49fda73d62e58f70aab2703b0edc3ea8547c655021858f98437ee790",
                "slot": 987433,
                "top": false,
                "transactions": [
                    "0x02f876018204dbd40c45bf2105dd18711a0082d208949da2816f6611bcab0cde5076c5f4e4a269e79bef8904563918244f40111180c080a0ee840d80915c9b506537909a5a6cf1ca2c5b47140d6585adab6ec0faf75fdcb7a07692785c5cb43c7cf02b800f"
                ]
                },
                "signature": "0x1b68ac14b663c9fc5b50984123ec9534bbd9cceda73e670af888da41af171505cc411d61252fb6cb3fa0017b679f8bb2305b26a285fa2737f175668d0dff91cc1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505"
            }
            ]"#
    }
}
