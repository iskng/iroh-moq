#![allow(unused_imports)] // Temp while developing
use anyhow::{ Result, bail, anyhow };
use iroh::{ Endpoint, SecretKey, NodeId };
use iroh::protocol::Router;
use iroh_moq::moq::protocol::MoqIroh;
use iroh_moq::moq::proto::{ AudioInit, AudioChunk };
use iroh_moq::moq::audio::{ AudioSource, AudioFrame, AudioConfig, AudioStreaming };
use rand::rngs::OsRng;
use std::sync::Arc;
use std::time::{ Duration, SystemTime, UNIX_EPOCH };
use tokio;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{ info, error, warn, debug };
use tracing_subscriber;

// A simple audio source that generates silence
struct SilenceSource {
    config: AudioConfig,
    frame_duration: Duration,
    frame_size_bytes: usize,
}

impl SilenceSource {
    fn new(config: AudioConfig) -> Self {
        // Calculate frame duration based on sample rate or config hint
        let frame_duration_us = config.frame_duration.unwrap_or_else(|| {
            // Default to 20ms frames if not specified
            (20_000.0 / (config.sample_rate as f64)) as u32
        });
        let frame_duration = Duration::from_micros(frame_duration_us as u64);

        // Calculate frame size: samples_per_frame * channels * bytes_per_sample (assuming 16-bit)
        let samples_per_frame = ((config.sample_rate as f64) *
            frame_duration.as_secs_f64()) as usize;
        let frame_size_bytes = samples_per_frame * (config.channels as usize) * 2; // 16-bit = 2 bytes

        Self {
            config,
            frame_duration,
            frame_size_bytes,
        }
    }
}

#[async_trait::async_trait]
impl AudioSource for SilenceSource {
    async fn next_frame(&mut self) -> Result<AudioFrame> {
        // Generate a frame of silence (zeroes)
        let data = vec![0u8; self.frame_size_bytes];
        let now = SystemTime::now();

        // Simulate frame generation time - sleep for frame duration
        tokio::time::sleep(self.frame_duration).await;

        Ok(AudioFrame {
            data,
            timestamp: now,
            duration: self.frame_duration,
        })
    }

    fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    fn channels(&self) -> u8 {
        self.config.channels
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::filter::EnvFilter
        ::new("info")
        .add_directive("iroh_moq=debug".parse().unwrap());
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter).with_target(true).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let mut rng = OsRng;
    let secret_key = SecretKey::generate(&mut rng);
    let node_id: NodeId = secret_key.public().into();
    println!("Audio Publisher node ID: {}", node_id);
    println!("To connect, run the subscriber with: cargo run --example audio_subscriber -- {}", node_id);
    info!("Publisher node ID: {}", node_id);

    let discovery = iroh::discovery::ConcurrentDiscovery::from_services(
        vec![
            Box::new(iroh::discovery::dns::DnsDiscovery::n0_dns()),
            Box::new(iroh::discovery::pkarr::PkarrPublisher::n0_dns(secret_key.clone()))
        ]
    );

    let endpoint = Endpoint::builder()
        .secret_key(secret_key.clone())
        .discovery(Box::new(discovery))
        .alpns(vec![iroh_moq::moq::proto::ALPN.to_vec(), iroh_gossip::ALPN.to_vec()])
        .bind().await?;

    let gossip = Arc::new(iroh_gossip::net::Gossip::builder().spawn(endpoint.clone()).await?);
    let moq = MoqIroh::builder().spawn(endpoint.clone(), gossip.clone()).await?;
    let client = moq.client();

    let router = Router::builder(endpoint.clone())
        .accept(iroh_moq::moq::proto::ALPN, moq)
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn().await?;
    let _router = Arc::new(router);

    let audio_config = AudioConfig {
        codec: "opus".to_string(),
        mime_type: "audio/opus".to_string(),
        bitrate: 64_000,
        sample_rate: 48_000,
        channels: 1, // Mono
        frame_duration: Some(20_000), // 20ms frames
    };

    let audio_init = AudioInit {
        codec: audio_config.codec.clone(),
        mime_type: audio_config.mime_type.clone(),
        sample_rate: audio_config.sample_rate,
        channels: audio_config.channels,
        bitrate: audio_config.bitrate,
        init_segment: vec![0; 4], // Placeholder Opus header
    };

    let namespace = "/live/audio_test".to_string();

    let source = SilenceSource::new(audio_config.clone());

    info!("Publishing audio stream: {} with config {:?}", namespace, audio_config);

    // Start streaming audio using the extension trait
    let stream_handle_res = client.stream_audio(
        namespace.clone(),
        source,
        audio_config,
        audio_init.init_segment.clone()
    ).await;

    let stream_handle = match stream_handle_res {
        Ok(handle) => handle,
        Err(e) => {
            error!("Failed to start audio stream: {}", e);
            return Err(e);
        }
    };

    info!("Audio stream started. Press Ctrl+C to stop.");

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;
    info!("Ctrl+C received, shutting down.");

    // Optionally, signal the stream to stop if needed (depends on implementation)
    stream_handle.abort(); // Abort the streaming task

    Ok(())
}
