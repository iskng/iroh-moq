#![allow(unused_imports)] // Temp while developing
use anyhow::{anyhow, bail, Result};
use iroh::protocol::Router;
use iroh::{Endpoint, NodeId, SecretKey};
use iroh_moq::moq::audio::{AudioConfig, AudioFrame, AudioSource, AudioStreaming};
use iroh_moq::moq::proto::{AudioChunk, AudioInit};
use iroh_moq::moq::protocol::MoqIroh;
use rand::rngs::OsRng;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber;
use tracing_subscriber::EnvFilter;

// Added dependencies
use ezk_g722::libg722::{encoder::Encoder as G722Encoder, Bitrate};
use hound::{WavReader, WavSpec};
use std::fs::File;
use std::io::BufReader;

// Audio source reading from a WAV file and encoding to G.722
struct FileAudioSource {
    config: AudioConfig,
    frame_duration: Duration,
    samples_per_frame: usize,
    wav_reader: WavReader<BufReader<File>>,
    encoder: G722Encoder, // Using ezk_g722::Encoder
    pcm_buffer: Vec<i16>, // Buffer for one frame of PCM samples
}

impl FileAudioSource {
    fn new(file_path: &str, config: AudioConfig) -> Result<Self> {
        info!("Opening WAV file: {}", file_path);
        let reader = WavReader::open(file_path)?;
        let spec = reader.spec();

        // --- Validate WAV file format for G.722 ---
        if spec.sample_rate != 16000 {
            bail!(
                "WAV file must have a sample rate of 16000 Hz for G.722, got {}",
                spec.sample_rate
            );
        }
        if spec.channels != 1 {
            bail!("WAV file must be mono (1 channel), got {}", spec.channels);
        }
        if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
            bail!("WAV file must be 16-bit signed integer PCM");
        }
        // --- Validation End ---

        let frame_duration_us = config.frame_duration.unwrap_or(20_000); // Default 20ms
        let frame_duration = Duration::from_micros(frame_duration_us as u64);
        // Samples per frame at 16kHz
        let samples_per_frame = ((spec.sample_rate as f64) * frame_duration.as_secs_f64()) as usize;

        info!(
            "AudioSource configured for G.722: frame_duration={}ms, samples_per_frame={}",
            frame_duration.as_millis(),
            samples_per_frame
        );

        Ok(Self {
            config,
            frame_duration,
            samples_per_frame,
            wav_reader: reader,
            // Initialize ezk_g722 encoder using correct Bitrate enum
            encoder: G722Encoder::new(Bitrate::Mode1_64000, false, false),
            pcm_buffer: Vec::with_capacity(samples_per_frame),
        })
    }
}

#[async_trait::async_trait]
impl AudioSource for FileAudioSource {
    async fn next_frame(&mut self) -> Result<AudioFrame> {
        // Capture timestamp *before* reading/encoding
        let frame_timestamp = SystemTime::now();

        self.pcm_buffer.clear();

        // Read samples for one frame
        let mut samples_read = 0;
        for sample_result in self
            .wav_reader
            .samples::<i16>()
            .take(self.samples_per_frame)
        {
            let sample = sample_result?;
            self.pcm_buffer.push(sample);
            samples_read += 1;
        }

        // Check if we reached the end of the file
        if samples_read == 0 {
            bail!("End of audio file reached");
        }
        // Pad last frame if necessary
        if samples_read < self.samples_per_frame {
            self.pcm_buffer.resize(self.samples_per_frame, 0);
            warn!(
                "Padding last G.722 audio frame with silence (read {}, needed {})",
                samples_read, self.samples_per_frame
            );
        }

        // Encode PCM buffer to G.722 using ezk_g722
        let g722_data: Vec<u8> = self.encoder.encode(&self.pcm_buffer); // Assuming encode method signature

        // G.722 encodes 16-bit PCM to 4 bits per sample, so output is half the input sample count.
        let expected_bytes = self.samples_per_frame / 2;
        if g722_data.len() != expected_bytes {
            warn!(
                "G.722 encoder produced unexpected byte count: got {}, expected {}",
                g722_data.len(),
                expected_bytes
            );
        }

        // --- Simplified Pacing ---
        // Sleep *after* processing the frame for its duration
        tokio::time::sleep(self.frame_duration).await;
        // --- Pacing End ---

        debug!(
            "AudioSource: Sending frame: timestamp={:?}, duration={:?}",
            frame_timestamp, self.frame_duration
        ); // Log values before returning

        Ok(AudioFrame {
            data: g722_data,
            timestamp: frame_timestamp, // Use timestamp captured *before* sleep
            duration: self.frame_duration,
        })
    }

    fn sample_rate(&self) -> u32 {
        self.config.sample_rate // Should be 16000 from validation
    }

    fn channels(&self) -> u8 {
        self.config.channels // Should be 1 from validation
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::filter::EnvFilter::new("info")
        .add_directive("iroh_moq=debug".parse().unwrap());
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let mut rng = OsRng;
    let secret_key = SecretKey::generate(&mut rng);
    let node_id: NodeId = secret_key.public().into();
    println!("Audio Publisher node ID: {}", node_id);
    println!(
        "To connect, run the subscriber with: cargo run --example audio_subscriber -- {}",
        node_id
    );
    info!("Publisher node ID: {}", node_id);

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

    let router = Router::builder(endpoint.clone())
        .accept(iroh_moq::moq::proto::ALPN, moq)
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn()
        .await?;
    let _router = Arc::new(router);

    let audio_config = AudioConfig {
        codec: "g722".to_string(),
        mime_type: "audio/g722".to_string(),
        sample_rate: 16000,           // G.722 input sample rate
        channels: 1,                  // Mono
        bitrate: 64_000,              // Mode 1 bitrate
        frame_duration: Some(20_000), // Send 20ms chunks (320 PCM samples -> 160 G.722 bytes)
    };

    let audio_init = AudioInit {
        codec: audio_config.codec.clone(),
        mime_type: audio_config.mime_type.clone(),
        sample_rate: audio_config.sample_rate,
        channels: audio_config.channels,
        bitrate: audio_config.bitrate,
        init_segment: Vec::new(), // G.722 needs no init segment
    };

    let namespace = "audio".to_string(); // Changed namespace

    let sample_file_path = "sample_16k_mono.wav"; // Ensure this 16kHz file exists!
    let source = match FileAudioSource::new(sample_file_path, audio_config.clone()) {
        Ok(s) => s,
        Err(e) => {
            error!(
                "Failed to create audio source from file '{}': {}",
                sample_file_path, e
            );
            error!(
                "Please ensure '{}' exists and is a 16kHz, mono, 16-bit PCM WAV file.",
                sample_file_path
            );
            bail!("Audio source creation failed");
        }
    };

    info!(
        "Publishing G.722 stream: {} from {}",
        namespace, sample_file_path
    );

    let stream_handle_res = client
        .stream_audio(
            namespace.clone(),
            source,
            audio_config, // Pass the config again
            audio_init.init_segment.clone(),
        )
        .await;

    let stream_handle = match stream_handle_res {
        Ok(handle) => handle,
        Err(e) => {
            error!("Failed to start audio stream: {}", e);
            return Err(e);
        }
    };

    info!("Audio stream started. Press Ctrl+C to stop.");

    let shutdown_token = CancellationToken::new();
    let _shutdown_clone = shutdown_token.clone();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl+C received, shutting down gracefully...");
            shutdown_token.cancel();
        }
        result = stream_handle => {
            match result {
                Ok(Ok(_)) => info!("Audio streaming task finished naturally (end of file)."),
                Ok(Err(e)) => {
                    // Check if the error is the expected EOF error
                    if e.to_string().contains("End of audio file reached") {
                        info!("Audio source reached end of file as expected.");
                    } else {
                        error!("Audio streaming task failed unexpectedly: {}", e);
                    }
                },
                Err(e) => error!("Audio streaming task panicked or was cancelled: {}", e),
            }
            // Add a small delay to allow network stack to potentially send FIN messages
            info!("Allowing a moment for stream termination signals...");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    info!("Audio publisher finished.");

    Ok(())
}
