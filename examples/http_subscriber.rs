use anyhow::{ anyhow, bail, Result };
use clap::Parser;
use iroh::endpoint::{ RecvStream, SendStream }; // Needed for http_fetch types
use iroh::{ Endpoint, NodeId, SecretKey };
use iroh_moq::moq::proto::ALPN; // Import ALPN from the correct path
use rand::rngs::OsRng;
use std::str::FromStr;
// use std::sync::Arc; // Not needed for basic subscriber
use std::time::Duration;
use tokio::io::{ AsyncReadExt, AsyncWriteExt }; // Required for stream operations
use tokio;
use tracing::{ debug, error, info, trace };
use tracing_subscriber;

// Protocol-specific constants (mirroring protocol.rs)
const STREAM_TYPE_HTTP: u8 = 0x03;

#[derive(Parser, Debug)]
#[clap(author, version, about = "MOQ-Iroh HTTP subscriber", long_about = None)]
struct Args {
    /// Publisher node ID to connect to
    #[clap(index = 1)]
    publisher_id: String,

    /// Path to request from the publisher (e.g., /hello.txt)
    #[clap(short, long, default_value = "/hello.txt")]
    path: String,
}

// --------------------------------------------------------------------
// HTTP Fetch Logic (copied & adapted from subscriber.rs)
// --------------------------------------------------------------------

/// Send an ordinary HTTP/1.1 request to the publisher and return the raw
/// response bytes.
async fn http_fetch(
    endpoint: &Endpoint,
    publisher_id: NodeId,
    request: &[u8]
) -> anyhow::Result<Vec<u8>> {
    info!("Initiating HTTP fetch via publisher {}", publisher_id);

    // open QUIC connection using the MoQ ALPN
    let conn = endpoint.connect(publisher_id, ALPN).await?;
    info!("Connected to publisher for HTTP fetch.");

    // open a bi-directional QUIC stream
    let (mut send, mut recv) = conn.open_bi().await?;
    info!("Opened bi-directional stream for HTTP fetch.");

    // 2a. announce the new stream-type
    send.write_all(&[STREAM_TYPE_HTTP]).await?;
    debug!("Sent STREAM_TYPE_HTTP byte.");

    // 2b. send the HTTP request verbatim
    send.write_all(request).await?;
    debug!("Sent {} request bytes.", request.len());

    // 2c. finish the write side to signal end of request
    send.finish()?;
    debug!("Finished send side of the stream.");

    // 2d. collect the response
    let mut resp_bytes = Vec::new();
    let mut total_resp_read = 0;
    let max_response_size = 10 * 1024 * 1024; // e.g., 10 MiB limit

    info!("Reading HTTP response...");
    // Limit read duration
    match
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            // Use read_chunk instead of read to avoid linter type confusion
            while let Some(chunk) = recv.read_chunk(64 * 1024, false).await? {
                let n = chunk.bytes.len();
                total_resp_read += n;
                if total_resp_read > max_response_size {
                    bail!("HTTP response exceeds maximum size of {} bytes", max_response_size);
                }
                resp_bytes.extend_from_slice(&chunk.bytes);
                trace!("Read {} response bytes (total {})", n, total_resp_read);
            }
            Ok::<(), anyhow::Error>(())
        }).await
    {
        Ok(Ok(_)) => {/* Read finished */}
        Ok(Err(e)) => {
            // Error during read
            return Err(e);
        }
        Err(_) => {
            // Timeout
            // Close the connection explicitly on timeout to signal the publisher
            conn.close((1u32).into(), b"response read timeout");
            bail!("Timeout reading HTTP response from {}", publisher_id);
        }
    }

    info!("Finished reading HTTP response ({} bytes).", resp_bytes.len());
    Ok(resp_bytes)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    let filter = tracing_subscriber::filter::EnvFilter
        ::new("info")
        .add_directive("iroh_moq=debug".parse().unwrap());
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter).with_target(true).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Parse command-line arguments
    let args = Args::parse();
    let publisher_id = NodeId::from_str(&args.publisher_id).map_err(|_|
        anyhow!("Invalid node ID format")
    )?;
    let requested_path = args.path;

    info!("Starting HTTP Subscriber...");
    info!("Attempting to connect to publisher: {}", publisher_id);
    info!("Requesting path: {}", requested_path);

    // Setup subscriber peer
    let mut rng = OsRng;
    let secret_key = SecretKey::generate(&mut rng);
    let node_id: NodeId = secret_key.public().into();
    info!("Subscriber node ID: {}\n", node_id); // Added newline for clarity

    // Setup discovery (needed to find the publisher)
    let discovery = iroh::discovery::ConcurrentDiscovery::from_services(
        vec![
            // Use Pkarr for fetching publisher records from DNS
            Box::new(iroh::discovery::pkarr::PkarrResolver::n0_dns()),
            // Use N0 DNS discovery
            Box::new(iroh::discovery::dns::DnsDiscovery::n0_dns())
        ]
    );

    // Build Iroh endpoint with discovery
    let endpoint = Endpoint::builder()
        .secret_key(secret_key.clone())
        .discovery(Box::new(discovery)) // Add discovery
        .alpns(vec![ALPN.to_vec()]) // Only need MoQ ALPN
        .bind().await?;

    info!("Subscriber endpoint bound with discovery.");

    // Craft the HTTP request
    // Using Connection: close simplifies response reading
    let http_request_str =
        format!("GET {} HTTP/1.1\r\nHost: iroh-http-proxy\r\nConnection: close\r\nUser-Agent: iroh-moq-http-subscriber/0.1\r\n\r\n", requested_path);
    let http_request_bytes = http_request_str.as_bytes();

    // Perform the HTTP fetch via the publisher
    match http_fetch(&endpoint, publisher_id, http_request_bytes).await {
        Ok(response_bytes) => {
            info!("HTTP fetch successful ({} bytes received).", response_bytes.len());
            // Attempt to print as UTF-8, but handle potential binary data gracefully
            // Check UTF-8 validity without moving the bytes
            match std::str::from_utf8(&response_bytes) {
                Ok(response_str) => {
                    println!("--- Response Start ---");
                    println!("{}", response_str.trim_end()); // Trim trailing whitespace/newlines
                    println!("--- Response End ---");
                }
                Err(e) => {
                    // If it fails, print the valid part and indicate error
                    let valid_len = e.valid_up_to();
                    println!("--- Response Start (Partially Decoded) ---");
                    // Safely print the valid UTF-8 prefix
                    if let Ok(valid_part) = std::str::from_utf8(&response_bytes[..valid_len]) {
                        print!("{}", valid_part);
                    }
                    println!("\n--- ERROR: Invalid UTF-8 sequence found at byte {} ---", valid_len);
                    println!("--- Response End (Binary Data may follow) ---");
                    // Optionally hex dump the rest or indicate binary nature
                    if response_bytes.len() > valid_len {
                        println!(
                            "Remaining non-UTF8 bytes: {} bytes",
                            response_bytes.len() - valid_len
                        );
                    }
                }
            }
        }
        Err(e) => {
            error!("HTTP fetch failed: {}", e);
            // Exit with an error code if the fetch fails
            return Err(e);
        }
    }

    // Close endpoint before exiting
    endpoint.close();
    info!("HTTP Subscriber finished.");
    Ok(())
}
