use anyhow::{ anyhow, bail, Result };
use iroh::endpoint::{ RecvStream, SendStream };
use iroh::protocol::{ ProtocolHandler, Router };
use iroh::{ Endpoint, NodeId, SecretKey };
use iroh_gossip::net::Gossip;
use iroh_moq::moq::proto::ALPN; // Import ALPN from the correct path
use iroh_moq::moq::protocol::MoqIroh;
use rand::rngs::OsRng;
use std::sync::Arc;
use tokio::io::AsyncWriteExt; // Required for write_all
use tokio;
use tokio_util::sync::CancellationToken;
use tracing::{ debug, error, info, trace, warn };
use tracing_subscriber;
use futures::future::BoxFuture; // Required for ProtocolHandler trait

// Protocol-specific constants (mirroring protocol.rs)
const STREAM_TYPE_CONTROL: u8 = 0x01;
const STREAM_TYPE_DATA: u8 = 0x02;
const STREAM_TYPE_HTTP: u8 = 0x03;

// --- HTTP Handling Logic (copied from protocol.rs adaptation) ---
async fn handle_http_stream(mut send: SendStream, mut recv: RecvStream) -> anyhow::Result<()> {
    let mut req_bytes = Vec::new();
    let mut total_read = 0;
    let max_request_size = 16 * 1024; // 16 KiB limit

    while let Some(chunk) = recv.read_chunk(1024, false).await? {
        total_read += chunk.bytes.len();
        if total_read > max_request_size {
            bail!("HTTP request exceeds max size of {} bytes", max_request_size);
        }
        req_bytes.extend_from_slice(&chunk.bytes);
    }
    // recv.finish().await?; // Removed: RecvStream is finished implicitly by reading EOF or error

    if req_bytes.is_empty() {
        // Handle empty request gracefully, maybe send 400 Bad Request
        let (status_code, message) = ("400 Bad Request", "Empty request.");
        let body = message.as_bytes();
        let header = format!(
            "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status_code,
            body.len()
        );
        send.write_all(header.as_bytes()).await?;
        send.write_all(body).await?;
        send.finish()?; // Removed .await
        return Ok(()); // Return Ok after sending error response
    }

    let req_str = String::from_utf8(req_bytes)?;
    info!("Received HTTP request:\n---\n{}---", req_str.trim());

    let path = req_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .ok_or_else(|| anyhow!("malformed request line"))?;

    let sanitized_path = path.trim_start_matches('/');
    if sanitized_path.contains("..") {
        bail!("invalid path contains '..'");
    }

    let file_path = format!("www/{}", sanitized_path);
    info!("Attempting to serve file: {}", file_path);

    match tokio::fs::read(&file_path).await {
        Ok(body) => {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            send.write_all(header.as_bytes()).await?;
            send.write_all(&body).await?;
            info!("Successfully sent file: {}", file_path);
        }
        Err(e) => {
            error!("Failed to read file {}: {}", file_path, e);
            let (status_code, message) = if e.kind() == std::io::ErrorKind::NotFound {
                ("404 Not Found", "File not found.")
            } else {
                ("500 Internal Server Error", "Failed to read file.")
            };
            let body = message.as_bytes();
            let header = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status_code,
                body.len()
            );
            send.write_all(header.as_bytes()).await?;
            send.write_all(body).await?;
        }
    }
    send.finish()?; // Removed .await
    debug!("Finished sending HTTP response for {}", path);
    Ok(())
}

// --- Custom Protocol Handler Wrapper ---
// We need a simple wrapper to implement ProtocolHandler if MoqIroh itself doesn't handle HTTP directly
#[derive(Clone, Debug)]
struct HttpHandler {
    moq_handler: MoqIroh, // Delegate other types to the main MoQ handler
}

impl ProtocolHandler for HttpHandler {
    fn accept(&self, conn: iroh::endpoint::Connection) -> BoxFuture<'static, Result<()>> {
        let moq_handler_clone = self.moq_handler.clone();
        Box::pin(async move {
            let remote_id = conn.remote_node_id()?;
            info!("HTTP_PUBLISHER: Accepted connection from: {}", remote_id);

            while let Ok((send, mut recv)) = conn.accept_bi().await {
                let mut stream_type_buf = [0u8; 1];
                match recv.read_exact(&mut stream_type_buf).await {
                    Ok(_) => {
                        let stream_type = stream_type_buf[0];
                        match stream_type {
                            STREAM_TYPE_HTTP => {
                                info!("Handling incoming HTTP stream from {}", remote_id);
                                tokio::spawn(async move {
                                    if let Err(e) = handle_http_stream(send, recv).await {
                                        error!("HTTP proxy stream handler failed: {}", e);
                                    }
                                });
                            }
                            // Delegate other stream types (CONTROL, DATA) to the MoqIroh handler
                            STREAM_TYPE_CONTROL | STREAM_TYPE_DATA => {
                                info!("Delegating stream type {} to MoqIroh handler", stream_type);
                                // To delegate properly, we'd need to reconstruct the stream
                                // with the type byte prepended or modify MoqIroh's accept.
                                // For simplicity here, we'll just log and drop.
                                // A more robust solution involves modifying MoqIroh::accept.
                                warn!("Received stream type {} - Dropping as delegation is complex.", stream_type);
                                // In a real scenario, you'd pass this off to moq_handler_clone.accept_stream(conn, stream_type, send, recv).await;
                                // This requires MoqIroh to expose such a method.
                                // Or, modify MoqIroh's accept to include the HTTP case.
                            }
                            unknown => {
                                error!("Unknown stream type received: {}", unknown);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to read stream type from {}: {}", remote_id, e);
                        break; // Stop handling streams for this connection if we can't read type
                    }
                }
            }
            info!("Finished handling connection from {}", remote_id);
            Ok(())
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::filter::EnvFilter
        ::new("info")
        .add_directive("iroh_moq=debug".parse().unwrap()); // Enable debug for our crate
    let subscriber = tracing_subscriber
        ::fmt()
        .with_env_filter(filter)
        .with_target(true) // Show module paths
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting HTTP Publisher...");

    // --- Create 'www' directory if it doesn't exist ---
    let www_dir = "www";
    match tokio::fs::create_dir(www_dir).await {
        Ok(_) => info!("Created directory '{}'", www_dir),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            info!("Directory '{}' already exists", www_dir);
        }
        Err(e) => bail!("Failed to create directory '{}': {}", www_dir, e),
    }
    // --- Create a sample file for testing ---
    let sample_file_path = format!("{}/hello.txt", www_dir);
    let sample_content = b"Hello from your friendly iroh-moq HTTP publisher!";
    match tokio::fs::write(&sample_file_path, sample_content).await {
        Ok(_) => info!("Created/updated sample file '{}'", sample_file_path),
        Err(e) => warn!("Failed to create/update sample file '{}': {}", sample_file_path, e),
    }

    // --- Standard Iroh Node Setup ---
    let mut rng = OsRng;
    let secret_key = SecretKey::generate(&mut rng);
    let node_id: NodeId = secret_key.public().into();
    println!("HTTP Publisher Node ID: {}", node_id);
    println!("\nRun the subscriber with:");
    println!("cargo run --example http_subscriber -- {}", node_id);
    println!("\nOptionally specify a path: cargo run --example http_subscriber -- {} -p /hello.txt\n", node_id);
    info!("Publisher Node ID: {}", node_id);

    // Use Pkarr for discovery
    let discovery = iroh::discovery::ConcurrentDiscovery::from_services(
        vec![
            Box::new(iroh::discovery::pkarr::PkarrPublisher::n0_dns(secret_key.clone())),
            Box::new(iroh::discovery::dns::DnsDiscovery::n0_dns()) // Also listen on DNS
        ]
    );

    let endpoint = Endpoint::builder()
        .secret_key(secret_key.clone())
        .discovery(Box::new(discovery))
        // We only need the ALPN for MoQ, as HTTP is tunneled *inside* MoQ connections
        .alpns(vec![ALPN.to_vec()])
        .bind().await?;
    info!("Publisher endpoint bound successfully.");

    // Setup Gossip (needed by MoqIroh internally, even if not used directly for HTTP)
    let gossip = Arc::new(iroh_gossip::net::Gossip::builder().spawn(endpoint.clone()).await?);
    info!("Gossip protocol started.");

    // Setup MoqIroh (needed for the ALPN and potentially internal state)
    let moq = MoqIroh::builder().spawn(endpoint.clone(), gossip.clone()).await?; // Use the created gossip
    info!("MoqIroh protocol handler created.");

    // --- Setup Router with our custom handler ---
    let http_handler = HttpHandler { moq_handler: moq }; // Wrap MoqIroh
    let router = Router::builder(endpoint.clone())
        // Use the custom handler for the MoQ ALPN
        .accept(ALPN, http_handler)
        // Still accept gossip ALPN if needed elsewhere
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn().await?;
    info!("Router started, accepting connections with ALPN: {:?}", ALPN);

    println!("Publisher listening... Press Ctrl+C to stop.");

    // Keep the publisher running until Ctrl+C
    let shutdown_token = CancellationToken::new();
    let _shutdown_clone = shutdown_token.clone();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl+C received, shutting down...");
            shutdown_token.cancel();
        }
        // Optional: Add a task here that the router might finish if applicable
        // router_result = router => { info!("Router task finished: {:?}", router_result); }
    }

    // Close endpoint gracefully
    endpoint.close(); // Removed arguments, .await, and ?
    info!("Publisher finished.");
    Ok(())
}
