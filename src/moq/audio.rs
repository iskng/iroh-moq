use crate::moq::client::MoqIrohClient;
use crate::moq::proto::{AudioChunk, AudioInit, MoqObject, MEDIA_TYPE_AUDIO, MEDIA_TYPE_EOF};
use anyhow::Result;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{error, info};
use uuid::Uuid;

/// Trait representing a source of encoded audio frames.
#[async_trait::async_trait]
pub trait AudioSource {
    /// Returns the next encoded audio frame.
    async fn next_frame(&mut self) -> Result<AudioFrame>;
    /// Sample rate in Hz
    fn sample_rate(&self) -> u32;
    /// Channel count
    fn channels(&self) -> u8;
}

/// A single encoded audio frame
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub data: Vec<u8>,
    pub timestamp: SystemTime,
    pub duration: Duration,
}

/// Configuration parameters for an audio stream
#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub codec: String,
    pub mime_type: String,
    pub bitrate: u32,
    pub sample_rate: u32,
    pub channels: u8,
    /// Optional frame duration hint (µs). If not provided, derived from sample_rate.
    pub frame_duration: Option<u32>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            codec: "opus".into(),
            mime_type: "audio/opus".into(),
            bitrate: 128_000,
            sample_rate: 48_000,
            channels: 2,
            frame_duration: None,
        }
    }
}

/// Handle returned when publishing an audio stream
pub struct AudioStreamHandle {
    pub stream_id: Uuid,
    chunk_sender: mpsc::Sender<AudioChunk>,
    frame_count: u64,
    config: AudioConfig,
}

impl AudioStreamHandle {
    pub async fn send_frame(&mut self, frame: AudioFrame) -> Result<()> {
        self.frame_count += 1;
        let timestamp = frame.timestamp.duration_since(UNIX_EPOCH)?.as_micros() as u64;
        let duration_us = frame.duration.as_micros() as u32;
        let chunk = AudioChunk {
            timestamp,
            duration: duration_us,
            is_key: true, // audio generally treat every frame as key
            data: frame.data,
        };
        self.chunk_sender.send(chunk).await?;
        Ok(())
    }
}

/// Extension trait to make `MoqIrohClient` capable of publishing audio streams.
#[async_trait::async_trait]
pub trait AudioStreaming {
    async fn stream_audio(
        &self,
        namespace: String,
        source: impl AudioSource + Send + 'static,
        config: AudioConfig,
        init_data: Vec<u8>,
    ) -> Result<JoinHandle<Result<()>>>;
}

#[async_trait::async_trait]
impl AudioStreaming for MoqIrohClient {
    async fn stream_audio(
        &self,
        namespace: String,
        mut source: impl AudioSource + Send + 'static,
        config: AudioConfig,
        init_data: Vec<u8>,
    ) -> Result<JoinHandle<Result<()>>> {
        let init = AudioInit {
            codec: config.codec.clone(),
            mime_type: config.mime_type.clone(),
            sample_rate: config.sample_rate,
            channels: config.channels,
            bitrate: config.bitrate,
            init_segment: init_data.clone(),
        };

        let (stream_id, object_sender) = self.publish_audio_stream(namespace.clone(), init).await?;
        info!("Audio stream published with ID: {}", stream_id);

        let handle = tokio::spawn(async move {
            let mut sequence: u64 = 1; // Start sequence after init (assumed seq 0)
            loop {
                match source.next_frame().await {
                    Ok(frame) => {
                        let timestamp_us =
                            frame.timestamp.duration_since(UNIX_EPOCH)?.as_micros() as u64;
                        let object = MoqObject {
                            name: format!("audio-seg-{}", sequence), // Example name
                            sequence,
                            timestamp: timestamp_us,
                            group_id: MEDIA_TYPE_AUDIO as u32,
                            priority: 128, // Default priority
                            data: frame.data,
                        };
                        if let Err(e) = object_sender.send(object).await {
                            error!("Failed to send audio object: {}", e);
                            break;
                        }
                        sequence += 1;
                    }
                    Err(e) => {
                        if e.to_string().contains("End of audio file reached") {
                            info!("AudioSource reported EOF; sending EOS marker.");
                            let eos_object = MoqObject {
                                name: format!("audio-eos-{}", sequence),
                                sequence,
                                timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros()
                                    as u64,
                                group_id: MEDIA_TYPE_EOF, // Use the EOS marker
                                priority: 255,            // High priority
                                data: Vec::new(),         // No data needed for EOS
                            };
                            if let Err(send_err) = object_sender.send(eos_object).await {
                                error!("Failed to send EOS marker: {}", send_err);
                            }
                            break; // Exit loop after sending EOS
                        } else {
                            error!("AudioSource error: {}", e);
                            // Decide if error is fatal or recoverable
                            // For now, we break on any other error
                            break;
                        }
                    }
                }
            }
            info!("Audio stream loop completed for stream {}", stream_id);
            // Sender is dropped automatically when task finishes
            Ok(())
        });
        Ok(handle)
    }
}
