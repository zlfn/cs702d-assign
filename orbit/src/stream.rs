use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::{ClientConfig, Connection, Endpoint, ServerConfig};
use rkyv::rancor::BoxedError;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

use crate::usage::EpochUsageTable;

/// Error types for Orbit stream operations
#[derive(Debug, thiserror::Error)]
pub enum OrbitStreamError {
    #[error("Connection error: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("Connect error: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("Write error: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("Read error: {0}")]
    Read(#[from] quinn::ReadError),
    #[error("Read exact error: {0}")]
    ReadExact(#[from] quinn::ReadExactError),
    #[error("Read to end error: {0}")]
    ReadToEnd(#[from] quinn::ReadToEndError),
    #[error("Closed stream error: {0}")]
    ClosedStream(#[from] quinn::ClosedStream),
    #[error("Serialization error: {0}")]
    Serialization(BoxedError),
    #[error("Deserialization error: {0}")]
    Deserialization(BoxedError),
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("Certificate generation error: {0}")]
    CertGen(#[from] rcgen::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Connection timeout")]
    Timeout,
    #[error("Epoch mismatch: requested {requested}, server has {server}")]
    EpochMismatch { requested: u64, server: u64 },
    #[error("Table not exist for epoch {0}: Not calculated or already pulled")]
    TableNotExist(u64),
}

/// Trait for Orbit node server functionality.
/// Implement this trait to provide usage table data to requesting clients.
pub trait OrbitNode: Send + Sync + 'static {
    /// Called when a client requests the usage table for a specific epoch.
    /// Returns the [`OrbitStreamError::EpochMismatch`] if EpochMismatch.
    /// Returns the [`OrbitStreamError::TableNotExist`] error if the current epoch has already been pulled by
    /// another node.
    fn get_usage_table(
        &self,
        epoch: u64,
    ) -> impl std::future::Future<Output = Result<EpochUsageTable, OrbitStreamError>> + Send;
}

/// QUIC client for sending UsageEpochTable to the next node in the ring.
pub struct OrbitStream {
    connection: Connection,
}

impl OrbitStream {
    /// Create a new OrbitStream client connected to the specified address.
    /// Uses self-signed certificates for simplicity (in production, use proper certs).
    /// `timeout` specifies the connection attempt timeout (not idle timeout).
    pub async fn connect(addr: SocketAddr, timeout: Duration) -> Result<Self, OrbitStreamError> {
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())?;
        endpoint.set_default_client_config(Self::insecure_client_config());

        let connection = tokio::time::timeout(timeout, endpoint.connect(addr, "orbit.local")?)
            .await
            .map_err(|_| OrbitStreamError::Timeout)??;

        Ok(Self { connection })
    }

    /// Connect using an existing endpoint
    pub async fn connect_with_endpoint(
        endpoint: &Endpoint,
        addr: SocketAddr,
    ) -> Result<Self, OrbitStreamError> {
        let connection = endpoint.connect(addr, "orbit.local")?.await?;
        Ok(Self { connection })
    }

    /// Request and receive a UsageEpochTable from the connected node for a specific epoch.
    /// Returns EpochMismatch error if the server's epoch doesn't match.
    pub async fn get_usage_table(&self, epoch: u64) -> Result<EpochUsageTable, OrbitStreamError> {
        let (mut send, mut recv) = self.connection.open_bi().await?;

        // Send epoch as request
        send.write_all(&epoch.to_be_bytes()).await?;
        send.finish()?;

        // Read response status (0 = success, 1 = epoch mismatch)
        let mut status_buf = [0u8; 1];
        recv.read_exact(&mut status_buf).await?;

        if status_buf[0] == 1 {
            // Epoch mismatch - read server's epoch
            let mut server_epoch_buf = [0u8; 8];
            recv.read_exact(&mut server_epoch_buf).await?;
            let server_epoch = u64::from_be_bytes(server_epoch_buf);
            return Err(OrbitStreamError::EpochMismatch {
                requested: epoch,
                server: server_epoch,
            });
        }

        // Read response
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let response_bytes = recv.read_to_end(len).await?;

        // Deserialize response
        let response = rkyv::from_bytes::<EpochUsageTable, BoxedError>(&response_bytes)
            .map_err(OrbitStreamError::Deserialization)?;

        Ok(response)
    }

    /// Close the connection gracefully.
    pub fn close(&self) {
        self.connection.close(0u32.into(), b"done");
    }

    /// Create an insecure client config that accepts any certificate.
    /// For development/testing only.
    fn insecure_client_config() -> ClientConfig {
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();

        ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap(),
        ))
    }
}

/// QUIC server that accepts connections and delegates to OrbitNode implementation.
pub struct OrbitServer {
    endpoint: Endpoint,
}

impl OrbitServer {
    /// Create a new OrbitServer bound to the specified address.
    pub fn new(addr: SocketAddr) -> Result<Self, OrbitStreamError> {
        let (server_config, _) = Self::generate_self_signed_config()?;
        let endpoint = Endpoint::server(server_config, addr)?;
        Ok(Self { endpoint })
    }

    /// Create with custom server config
    pub fn with_config(addr: SocketAddr, config: ServerConfig) -> Result<Self, OrbitStreamError> {
        let endpoint = Endpoint::server(config, addr)?;
        Ok(Self { endpoint })
    }

    /// Run the server, handling incoming connections with the provided OrbitNode.
    pub async fn run<N: OrbitNode>(self, node: Arc<N>) -> Result<(), OrbitStreamError> {
        tracing::info!("OrbitServer listening on {}", self.endpoint.local_addr()?);

        while let Some(incoming) = self.endpoint.accept().await {
            let node = Arc::clone(&node);
            tokio::spawn(async move {
                if let Err(e) = Self::handle_connection(incoming, node).await {
                    tracing::error!("Connection error: {}", e);
                }
            });
        }

        Ok(())
    }

    /// Handle a single incoming connection.
    async fn handle_connection<N: OrbitNode>(
        incoming: quinn::Incoming,
        node: Arc<N>,
    ) -> Result<(), OrbitStreamError> {
        let connection = incoming.await?;
        tracing::debug!("New connection from {}", connection.remote_address());

        loop {
            match connection.accept_bi().await {
                Ok((send, recv)) => {
                    let node = Arc::clone(&node);
                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_bi_stream(send, recv, node).await {
                            tracing::error!("Bi-stream error: {}", e);
                        }
                    });
                }
                Err(quinn::ConnectionError::ApplicationClosed(_)) => break,
                Err(e) => return Err(e.into()),
            }
        }

        Ok(())
    }

    /// Handle bidirectional stream (request-response pattern).
    /// Client sends epoch, server responds with usage table or epoch mismatch error.
    async fn handle_bi_stream<N: OrbitNode>(
        mut send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        node: Arc<N>,
    ) -> Result<(), OrbitStreamError> {
        // Read requested epoch from client
        let mut epoch_buf = [0u8; 8];
        recv.read_exact(&mut epoch_buf).await?;
        let requested_epoch = u64::from_be_bytes(epoch_buf);

        // Get usage table from node
        match node.get_usage_table(requested_epoch).await {
            Ok(table) => {
                // Success - send status 0 and table
                send.write_all(&[0u8]).await?;

                let response_bytes =
                    rkyv::to_bytes::<BoxedError>(&table).map_err(OrbitStreamError::Serialization)?;

                let len = response_bytes.len() as u32;
                send.write_all(&len.to_be_bytes()).await?;
                send.write_all(&response_bytes).await?;
            }
            Err(OrbitStreamError::EpochMismatch { server, .. }) => {
                // Epoch mismatch - send status 1 and server's epoch
                send.write_all(&[1u8]).await?;
                send.write_all(&server.to_be_bytes()).await?;
            }
            Err(e) => return Err(e),
        }

        send.finish()?;
        Ok(())
    }

    /// Generate a self-signed certificate for testing.
    fn generate_self_signed_config() -> Result<(ServerConfig, Vec<u8>), OrbitStreamError> {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
        let cert_der = CertificateDer::from(cert.cert);
        let key_der = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());

        let mut server_config =
            ServerConfig::with_single_cert(vec![cert_der.clone()], key_der.into())?;

        // Configure transport
        let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
        transport_config.max_idle_timeout(Some(
            std::time::Duration::from_secs(30).try_into().unwrap(),
        ));

        Ok((server_config, cert_der.to_vec()))
    }

    /// Get the local address the server is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, OrbitStreamError> {
        Ok(self.endpoint.local_addr()?)
    }
}

/// Custom certificate verifier that skips verification (for development only).
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestNode {
        current_epoch: u64,
    }

    impl OrbitNode for TestNode {
        async fn get_usage_table(&self, epoch: u64) -> Result<EpochUsageTable, OrbitStreamError> {
            if epoch != self.current_epoch {
                return Err(OrbitStreamError::EpochMismatch {
                    requested: epoch,
                    server: self.current_epoch,
                });
            }
            // Return a test table
            let mut table = EpochUsageTable::new(self.current_epoch, 10);
            table.push_epoch(crate::usage::EpochUsage::from_vec(
                42,
                vec![(uuid::Uuid::new_v4().to_bytes_le(), 50)],
            ));
            Ok(table)
        }
    }

    #[tokio::test]
    async fn test_client_server_roundtrip() {
        let _ = tracing_subscriber::fmt::try_init();
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Start server
        let server = OrbitServer::new("127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server.local_addr().unwrap();

        let node = Arc::new(TestNode { current_epoch: 42 });
        tokio::spawn(async move {
            server.run(node).await.unwrap();
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Connect client
        let client = OrbitStream::connect(server_addr, Duration::from_secs(5)).await.unwrap();

        // Request usage table with correct epoch
        let response = client.get_usage_table(42).await.unwrap();

        assert_eq!(response.tables.len(), 1);
        assert_eq!(response.tables[0].epoch, 42);
        assert_eq!(response.tables[0].usages[0].1, 50);

        client.close();
    }

    #[tokio::test]
    async fn test_epoch_mismatch() {
        let _ = tracing_subscriber::fmt::try_init();
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Start server
        let server = OrbitServer::new("127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server.local_addr().unwrap();

        let node = Arc::new(TestNode { current_epoch: 42 });
        tokio::spawn(async move {
            server.run(node).await.unwrap();
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Connect client
        let client = OrbitStream::connect(server_addr, Duration::from_secs(5)).await.unwrap();

        // Request usage table with wrong epoch
        let result = client.get_usage_table(99).await;

        match result {
            Err(OrbitStreamError::EpochMismatch { requested, server }) => {
                assert_eq!(requested, 99);
                assert_eq!(server, 42);
            }
            _ => panic!("Expected EpochMismatch error"),
        }

        client.close();
    }

    /// Test node that returns a large usage table with 1 million UUIDs across 10 epochs
    /// Table is pre-generated to measure pure transfer time
    struct LargeTestNode {
        current_epoch: u64,
        prebuilt_table: EpochUsageTable,
    }

    impl LargeTestNode {
        fn new(current_epoch: u64) -> Self {
            // Pre-generate table with 10 epochs, 1M UUIDs each (10M total)
            let mut table = EpochUsageTable::new(current_epoch, 11);
            for epoch_offset in 0..10 {
                let mut usages = Vec::with_capacity(1_000_000);
                for i in 0..1_000_000u64 {
                    // Generate deterministic UUID from index
                    let mut uuid_bytes = [0u8; 16];
                    uuid_bytes[0..8].copy_from_slice(&(epoch_offset as u64).to_le_bytes());
                    uuid_bytes[8..16].copy_from_slice(&i.to_le_bytes());
                    usages.push((uuid_bytes, i % 1000));
                }
                table.push_epoch(crate::usage::EpochUsage::from_vec(
                    current_epoch - epoch_offset,
                    usages,
                ));
            }
            Self {
                current_epoch,
                prebuilt_table: table,
            }
        }
    }

    impl OrbitNode for LargeTestNode {
        async fn get_usage_table(&self, epoch: u64) -> Result<EpochUsageTable, OrbitStreamError> {
            if epoch != self.current_epoch {
                return Err(OrbitStreamError::EpochMismatch {
                    requested: epoch,
                    server: self.current_epoch,
                });
            }
            Ok(self.prebuilt_table.clone())
        }
    }

    #[ignore]
    #[tokio::test]
    async fn test_large_table_transfer_benchmark() {
        let _ = tracing_subscriber::fmt::try_init();
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Start server with large test node
        let server = OrbitServer::new("127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server.local_addr().unwrap();

        println!("Building test table...");
        let build_start = std::time::Instant::now();
        let node = Arc::new(LargeTestNode::new(42));
        println!("Table built in {:?}", build_start.elapsed());
        tokio::spawn(async move {
            server.run(node).await.unwrap();
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Connect client
        let client = OrbitStream::connect(server_addr, Duration::from_secs(30)).await.unwrap();

        // Measure transfer time
        let start = std::time::Instant::now();
        let response = client.get_usage_table(42).await.unwrap();
        let elapsed = start.elapsed();

        // Verify data
        assert_eq!(response.tables.len(), 10);
        let total_entries: usize = response.tables.iter().map(|t| t.usages.len()).sum();
        assert_eq!(total_entries, 10_000_000);

        // Measure individual operations
        let clone_start = std::time::Instant::now();
        let cloned = response.clone();
        let clone_elapsed = clone_start.elapsed();

        let ser_start = std::time::Instant::now();
        let serialized = rkyv::to_bytes::<rkyv::rancor::BoxedError>(&cloned).unwrap();
        let ser_elapsed = ser_start.elapsed();
        let data_size_mb = serialized.len() as f64 / 1_000_000.0;

        let deser_start = std::time::Instant::now();
        let _deserialized = rkyv::from_bytes::<EpochUsageTable, rkyv::rancor::BoxedError>(&serialized).unwrap();
        let deser_elapsed = deser_start.elapsed();

        println!("========================================");
        println!("Large Table Transfer Benchmark Results:");
        println!("========================================");
        println!("Epochs: 10");
        println!("UUIDs per epoch: 1,000,000");
        println!("Total entries: 10,000,000");
        println!("Data size: {:.2} MB", data_size_mb);
        println!("----------------------------------------");
        println!("Clone time: {:?}", clone_elapsed);
        println!("Serialization time: {:?}", ser_elapsed);
        println!("Deserialization time: {:?}", deser_elapsed);
        println!("Total transfer time: {:?}", elapsed);
        println!("Est. network time: {:?}", elapsed.saturating_sub(ser_elapsed).saturating_sub(deser_elapsed));
        println!("----------------------------------------");
        println!("Throughput: {:.2} entries/sec", total_entries as f64 / elapsed.as_secs_f64());
        println!("========================================");

        client.close();
    }
}
