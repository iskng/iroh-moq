use anyhow::Result;
use std::time::{ Duration, SystemTime, UNIX_EPOCH };
use tokio::{ sync::mpsc, task::JoinHandle };
use uuid::Uuid;
use crate::moq::proto::{ AudioInit, AudioChunk, MEDIA_TYPE_AUDIO };
use crate::moq::client::MoqIrohClient;
use tracing::{ info, error };

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
        init_data: Vec<u8>
    ) -> Result<JoinHandle<Result<()>>>;
}

#[async_trait::async_trait]
impl AudioStreaming for MoqIrohClient {
    async fn stream_audio(
        &self,
        namespace: String,
        mut source: impl AudioSource + Send + 'static,
        config: AudioConfig,
        init_data: Vec<u8>
    ) -> Result<JoinHandle<Result<()>>> {
        let init = AudioInit {
            codec: config.codec.clone(),
            mime_type: config.mime_type.clone(),
            sample_rate: config.sample_rate,
            channels: config.channels,
            bitrate: config.bitrate,
            init_segment: init_data.clone(),
        };

        let (stream_id, chunk_sender) = self.publish_audio_stream(namespace.clone(), init).await?;
        info!("Audio stream published with ID: {}", stream_id);

        let (request_tx, mut request_rx) = mpsc::channel::<()>(2);
        let _ = request_tx.send(()).await; // initial trigger

        let handle = tokio::spawn(async move {
            let mut frame_count: u64 = 0;
            loop {
                match request_rx.recv().await {
                    Some(_) => {
                        let frame = match source.next_frame().await {
                            Ok(f) => f,
                            Err(e) => {
                                error!("AudioSource error: {}", e);
                                continue;
                            }
                        };
                        let duration = frame.duration;
                        let timestamp_us = frame.timestamp
                            .duration_since(UNIX_EPOCH)?
                            .as_micros() as u64;
                        let chunk = AudioChunk {
                            timestamp: timestamp_us,
                            duration: duration.as_micros() as u32,
                            is_key: true,
                            data: frame.data,
                        };
                        if let Err(e) = chunk_sender.send(chunk).await {
                            error!("Failed to send audio chunk: {}", e);
                            break;
                        }
                        frame_count += 1;
                        let _ = request_tx.send(()).await; // request next
                    }
                    None => {
                        break;
                    }
                }
            }
            Ok(())
        });
        Ok(handle)
    }
}
