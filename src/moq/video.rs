use anyhow::Result;
use std::time::{ Duration, SystemTime, UNIX_EPOCH };
use tokio::{ sync::mpsc, task::JoinHandle };
use uuid::Uuid;
use crate::moq::proto::{ MediaInit, VideoChunk };
use crate::moq::client::MoqIrohClient;
use tracing::{ info, error };

/// Trait for video sources that can provide frames
#[async_trait::async_trait]
pub trait VideoSource {
    /// Get the next frame from the source
    async fn next_frame(&mut self) -> Result<VideoFrame>;

    /// Get the width of the video frames
    fn width(&self) -> usize;

    /// Get the height of the video frames
    fn height(&self) -> usize;
}

/// Represents a single video frame
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Raw frame data in BGRA format
    pub data: Vec<u8>,
    /// Frame width in pixels
    pub width: usize,
    /// Frame height in pixels
    pub height: usize,
    /// Frame timestamp
    pub timestamp: SystemTime,
}

/// Configuration for video streaming
#[derive(Debug, Clone)]
pub struct VideoConfig {
    /// Video codec to use (e.g. "avc1.64001f" for H.264)
    pub codec: String,
    /// MIME type of the video (e.g. "video/mp4")
    pub mime_type: String,
    /// Target bitrate in bits per second
    pub bitrate: u32,
    /// Target frame rate
    pub frame_rate: f32,
    /// Keyframe interval in frames
    pub keyframe_interval: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            codec: "avc1.64001f".to_string(),
            mime_type: "video/mp4".to_string(),
            bitrate: 5_000_000,
            frame_rate: 30.0,
            keyframe_interval: 30,
        }
    }
}

/// Handle for controlling a video stream
pub struct VideoStreamHandle {
    stream_id: Uuid,
    chunk_sender: mpsc::Sender<VideoChunk>,
    frame_count: u64,
    config: VideoConfig,
}

impl VideoStreamHandle {
    /// Send a video frame
    pub async fn send_frame(&mut self, frame: VideoFrame) -> Result<()> {
        self.frame_count += 1;

        let chunk = VideoChunk {
            timestamp: frame.timestamp.duration_since(UNIX_EPOCH)?.as_micros() as u64,
            duration: (1_000_000.0 / (self.config.frame_rate as f64)) as u32,
            is_keyframe: self.frame_count % (self.config.keyframe_interval as u64) == 1,
            dependency_sequence: None,
            data: frame.data,
        };

        self.chunk_sender.send(chunk).await?;
        Ok(())
    }

    /// Get the stream ID
    pub fn stream_id(&self) -> Uuid {
        self.stream_id
    }
}

/// Extension trait for MoqIrohClient to add video streaming capabilities
#[async_trait::async_trait]
pub trait VideoStreaming {
    /// Stream video from a source
    async fn stream_video(
        &self,
        namespace: String,
        source: impl VideoSource + Send + 'static,
        config: VideoConfig,
        init_data: Vec<u8>
    ) -> Result<JoinHandle<Result<()>>>;
}

#[async_trait::async_trait]
impl VideoStreaming for MoqIrohClient {
    async fn stream_video(
        &self,
        namespace: String,
        mut source: impl VideoSource + Send + 'static,
        config: VideoConfig,
        init_data: Vec<u8>
    ) -> Result<JoinHandle<Result<()>>> {
        // Create initialization with source dimensions
        let init = MediaInit {
            codec: config.codec.clone(),
            mime_type: config.mime_type.clone(),
            width: source.width() as u32,
            height: source.height() as u32,
            frame_rate: config.frame_rate,
            bitrate: config.bitrate,
            init_segment: init_data.clone(),
        };

        info!("Publishing video stream with resolution {}x{}", init.width, init.height);

        // Publish the video stream via the protocol layer
        // This creates a single stream ID used for both announcement and data flow
        let (stream_id, chunk_sender) = self.publish_video_stream(namespace.clone(), init).await?;

        info!("Video stream published with ID: {}", stream_id);

        // Create a channel for signaling the publisher to get the next frame
        // This enables pull-based frame acquisition instead of continuous pushing
        let (request_tx, mut request_rx) = mpsc::channel::<()>(2);

        // Send an initial request to get the first frame immediately
        let _ = request_tx.send(()).await;

        // Spawn a single task for video streaming with pull-based approach
        let handle = tokio::spawn(async move {
            let start_time = SystemTime::now();
            let mut frame_count = 0;
            let mut last_stats_time = start_time;
            let mut frames_since_last_stats = 0;

            // Create a small buffer of at most 2 frames to ensure smooth playback
            let mut frame_buffer: Option<VideoChunk> = None;

            loop {
                // Wait for a frame request signal
                match request_rx.recv().await {
                    Some(_) => {
                        // Get the next frame from the source (if buffer is empty)
                        if frame_buffer.is_none() {
                            let frame = match source.next_frame().await {
                                Ok(frame) => frame,
                                Err(e) => {
                                    error!("Failed to get next frame: {}", e);
                                    // Even on error, we still want to allow another request
                                    let _ = request_tx.send(()).await;
                                    continue;
                                }
                            };

                            // Create a video chunk from the frame
                            let is_keyframe = frame_count % (config.keyframe_interval as u64) == 0;
                            let timestamp = frame.timestamp
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_micros() as u64;

                            frame_buffer = Some(VideoChunk {
                                timestamp,
                                duration: (1_000_000.0 / (config.frame_rate as f64)) as u32,
                                is_keyframe,
                                dependency_sequence: None,
                                data: frame.data,
                            });
                        }

                        // Send the frame from the buffer
                        if let Some(chunk) = frame_buffer.take() {
                            // Send the chunk
                            if let Err(e) = chunk_sender.send(chunk).await {
                                error!("Failed to send frame: {}", e);
                                break;
                            }

                            // Update frame counting and stats
                            frame_count += 1;
                            frames_since_last_stats += 1;

                            // Start getting the next frame immediately while the current one is being processed
                            // This creates a pipeline effect to reduce latency
                            tokio::spawn({
                                let request_tx = request_tx.clone();
                                async move {
                                    // Small delay to avoid overwhelming the system
                                    tokio::time::sleep(Duration::from_millis(1)).await;
                                    let _ = request_tx.send(()).await;
                                }
                            });
                        }

                        // Log stats periodically
                        if frames_since_last_stats >= 30 || frame_count <= 1 {
                            if let Ok(now) = SystemTime::now().duration_since(last_stats_time) {
                                let fps = (frames_since_last_stats as f64) / now.as_secs_f64();
                                info!(
                                    "Video stream stats: {} frames sent ({:.2} fps), total: {}",
                                    frames_since_last_stats,
                                    fps,
                                    frame_count
                                );
                                frames_since_last_stats = 0;
                                last_stats_time = SystemTime::now();
                            }
                        }
                    }
                    None => {
                        // Channel closed, exit loop
                        info!("Frame request channel closed, ending stream");
                        break;
                    }
                }
            }

            if let Ok(duration) = SystemTime::now().duration_since(start_time) {
                info!(
                    "Video stream complete: sent {} frames in {:.2} seconds ({:.2} fps)",
                    frame_count,
                    duration.as_secs_f64(),
                    (frame_count as f64) / duration.as_secs_f64()
                );
            }
            Ok(())
        });

        Ok(handle)
    }
}
