use anyhow::{anyhow, Result};
use clap::Parser;
use futures::{pin_mut, StreamExt};
use iroh::protocol::Router;
use iroh::{Endpoint, NodeId, SecretKey};
use iroh_moq::moq::protocol::MoqIroh;
use rand::rngs::OsRng;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio;
use tracing::{debug, error, info, warn};
use tracing_subscriber;

#[derive(Parser, Debug)]
#[clap(author, version, about = "MOQ-Iroh audio subscriber", long_about = None)]
struct Args {
    /// Publisher node ID to connect to
    #[clap(index = 1)]
    publisher_id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    let filter = tracing_subscriber::filter::EnvFilter::new("info")
        .add_directive("iroh_moq=debug".parse().unwrap());
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Parse command-line arguments
    let args = Args::parse();
    let publisher_id =
        NodeId::from_str(&args.publisher_id).map_err(|_| anyhow!("Invalid node ID format"))?;

    // Setup subscriber peer
    info!("Setting up subscriber peer...");
    let mut rng = OsRng;
    let secret_key = SecretKey::generate(&mut rng);
    let node_id: NodeId = secret_key.public().into();
    info!("Subscriber node ID: {}", node_id);

    let discovery = iroh::discovery::ConcurrentDiscovery::from_services(vec![
        Box::new(iroh::discovery::dns::DnsDiscovery::n0_dns()),
        Box::new(iroh::discovery::pkarr::PkarrPublisher::n0_dns(
            secret_key.clone(),
        )),
    ]);

    let endpoint = Endpoint::builder()
        .secret_key(secret_key.clone())
        .discovery(Box::new(discovery))
        .alpns(vec![
            iroh_moq::moq::proto::ALPN.to_vec(),
            iroh_gossip::ALPN.to_vec(),
        ])
        .bind()
        .await?;

    let gossip = Arc::new(
        iroh_gossip::net::Gossip::builder()
            .spawn(endpoint.clone())
            .await?,
    );
    let moq = MoqIroh::builder()
        .spawn(endpoint.clone(), gossip.clone())
        .await?;
    let client = moq.client();

    let _router = Router::builder(endpoint.clone())
        .accept(iroh_moq::moq::proto::ALPN, moq.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn()
        .await?;
    let _router = Arc::new(_router);

    // Setup Ctrl+C handler
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
        info!("Ctrl+C received, shutting down.");
        let _ = shutdown_tx_clone.send(()).await;
    });

    info!("Connecting to publisher: {}", publisher_id);
    info!("Waiting for stream announcements...");

    let announcements = client.monitor_streams(publisher_id).await?;
    pin_mut!(announcements);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                info!("Shutdown signal received, exiting.");
                break;
            }
            announcement_opt = announcements.next() => {
                match announcement_opt {
                    Some(announcement) => {
                        info!("Received announcement: stream_id={}, namespace={}", announcement.stream_id, announcement.namespace);

                        // Only subscribe if namespace indicates audio
                        if announcement.namespace.contains("audio") {
                            info!("Attempting to subscribe to audio stream...");
                            let result = client.subscribe_to_audio_stream(announcement.clone()).await;

                            match result {
                                Ok((mut init_rx, mut chunk_rx)) => {
                                    info!("Successfully subscribed to audio stream {}", announcement.stream_id);

                                    // Handle init segment
                                    tokio::select!{
                                        init_opt = init_rx.recv() => {
                                            if let Some(init) = init_opt {
                                                 info!("Received AudioInit: codec={}, sample_rate={}, channels={}, init_size={}",
                                                       init.codec, init.sample_rate, init.channels, init.init_segment.len());
                                            } else {
                                                warn!("Init channel closed before receiving init segment.");
                                            }
                                        }
                                        _ = tokio::time::sleep(Duration::from_secs(5)) => {
                                            debug!("Timeout waiting for AudioInit segment (expected for empty init).");
                                        }
                                    }

                                    // Process audio chunks
                                    let mut chunk_count: u64 = 0;
                                    let mut total_bytes: usize = 0;
                                    let start_time = SystemTime::now();
                                    let mut latencies: Vec<Option<Duration>> = Vec::new();

                                    while let Some(Some(chunk)) = chunk_rx.recv().await {
                                        let recv_time = SystemTime::now();
                                        let send_time_result = UNIX_EPOCH.checked_add(
                                            Duration::from_millis(chunk.timestamp)
                                        );

                                        let latency = match send_time_result {
                                            Some(send_time) => recv_time.duration_since(send_time).ok(),
                                            None => {
                                                warn!("Failed to calculate send time from timestamp: {}", chunk.timestamp);
                                                None
                                            }
                                        };
                                        latencies.push(latency);

                                        chunk_count += 1;
                                        total_bytes += chunk.data.len();
                                        if chunk_count % 100 == 0 {
                                            info!(
                                                "Received audio chunk #{}: timestamp={}, duration={}us, size={}, latency={:?}",
                                                chunk_count,
                                                chunk.timestamp,
                                                chunk.duration,
                                                chunk.data.len(),
                                                latency.map(|d| d.as_micros())
                                            );
                                        }

                                        if shutdown_rx.try_recv().is_ok() {
                                            info!("Shutdown signal received during chunk processing.");
                                            return Ok(());
                                        }
                                    }
                                    info!("Audio chunk stream segment ended (EOS received).");

                                    let elapsed = start_time.elapsed().unwrap_or_default();

                                    // Calculate Bitrate and Latency Stats
                                    let avg_bitrate_bps = if !elapsed.is_zero() {
                                        (total_bytes * 8) as f64 / elapsed.as_secs_f64()
                                    } else {
                                        0.0
                                    };
                                    let valid_latencies: Vec<Duration> = latencies.into_iter().flatten().collect();
                                    let (min_latency, max_latency, avg_latency) = if !valid_latencies.is_empty() {
                                        let min = valid_latencies.iter().min().cloned();
                                        let max = valid_latencies.iter().max().cloned();
                                        let total_latency_micros: u128 = valid_latencies.iter().map(|d| d.as_micros()).sum();
                                        let avg = if valid_latencies.is_empty() { None } else {
                                            Some(Duration::from_micros((total_latency_micros / (valid_latencies.len() as u128)) as u64))
                                        };
                                        (min, max, avg)
                                    } else {
                                        (None, None, None)
                                    };

                                    info!(
                                        "Finished processing audio stream: {} chunks, {} bytes in {:.2?}s",
                                        chunk_count, total_bytes, elapsed
                                    );
                                    info!(
                                        "  Average Bitrate: {:.2} kbps", avg_bitrate_bps / 1000.0
                                    );
                                    info!(
                                        "  Latency (min/avg/max): {:?} / {:?} / {:?}",
                                        min_latency.map(|d| d.as_micros()),
                                        avg_latency.map(|d| d.as_micros()),
                                        max_latency.map(|d| d.as_micros())
                                    );

                                }
                                Err(e) => {
                                    error!("Failed to subscribe to audio stream {}: {}", announcement.stream_id, e);
                                }
                            }
                        } else {
                            debug!("Ignoring non-audio announcement for namespace: {}", announcement.namespace);
                        }
                    }
                    None => {
                        info!("Stream announcements ended.");
                        break;
                    }
                }
            }
        }
    }

    info!("Audio subscriber finished.");
    Ok(())
}
