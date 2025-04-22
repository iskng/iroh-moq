use anyhow::{ anyhow, bail, Result };
use ffmpeg::codec::{ encoder, packet, Context as CodecContext };
use ffmpeg::format::Pixel as FFmpegPixel;
use ffmpeg::software::scaling::{ context::Context as Scaler, flag::Flags };
use ffmpeg::util::frame::video::Video as FfmpegFrame;
use ffmpeg_next as ffmpeg;
use iroh::protocol::Router;
use iroh::{ Endpoint, NodeId, SecretKey };
use iroh_moq::moq::proto::{ MediaInit, VideoChunk };
use iroh_moq::moq::protocol::MoqIroh;
use iroh_moq::moq::video::VideoConfig;
use rand::rngs::OsRng;
use screencapturekit::output::MutLockTrait;
use screencapturekit::{
    output::CMSampleBuffer,
    shareable_content::SCShareableContent,
    stream::{
        configuration::{ pixel_format::PixelFormat, SCStreamConfiguration },
        content_filter::SCContentFilter,
        delegate_trait::SCStreamDelegateTrait,
        output_trait::SCStreamOutputTrait,
        output_type::SCStreamOutputType,
        SCStream,
    },
};
use std::sync::{ Arc, Mutex };
use std::time::{ Duration, SystemTime, UNIX_EPOCH };
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{ debug, error, info, warn };
use tracing_subscriber;

struct FrameHandler {
    sender: mpsc::Sender<CMSampleBuffer>,
}

impl SCStreamDelegateTrait for FrameHandler {}

impl SCStreamOutputTrait for FrameHandler {
    fn did_output_sample_buffer(
        &self,
        sample_buffer: CMSampleBuffer,
        _of_type: SCStreamOutputType
    ) {
        if let Err(e) = self.sender.try_send(sample_buffer) {
            warn!("Failed to send frame to channel (likely full or closed): {}", e);
        }
    }
}

struct ScreenCapturer {
    frame_rx: mpsc::Receiver<CMSampleBuffer>,
    width: usize,
    height: usize,
    stride: usize, // Bytes per row
    _stream: Arc<Mutex<SCStream>>,
}

impl ScreenCapturer {
    async fn new() -> Result<Self> {
        let content = SCShareableContent::get().map_err(|e|
            anyhow::anyhow!("Failed to get displays: {}", e)
        )?;
        let displays = content.displays();
        if displays.is_empty() {
            return Err(anyhow::anyhow!("No displays found"));
        }

        let display = &displays[0];
        let width = (display.width() & !1) as u32;
        let height = (display.height() & !1) as u32;

        let config = SCStreamConfiguration::new()
            .set_width(width)
            .map_err(|e| anyhow!("Failed to set width: {}", e))?
            .set_height(height)
            .map_err(|e| anyhow!("Failed to set height: {}", e))?
            .set_pixel_format(PixelFormat::BGRA)
            .map_err(|e| anyhow!("Failed to set pixel format: {}", e))?;

        let filter = SCContentFilter::new().with_display_excluding_windows(display, &[]);
        let (tx, mut rx) = mpsc::channel(64);

        let _delegate = FrameHandler { sender: tx.clone() };
        let mut stream = SCStream::new(&filter, &config);

        stream.add_output_handler(FrameHandler { sender: tx }, SCStreamOutputType::Screen);

        if let Err(e) = stream.start_capture() {
            return Err(anyhow::anyhow!("Failed to start capture: {}", e));
        }
        let display_id = display.display_id();
        info!("Screen capture started for display {} ({}x{})", display_id, width, height);

        let sample = rx.recv().await.ok_or_else(|| anyhow!("Frame channel closed prematurely"))?;
        let stride = {
            let mut pixel_buffer = sample
                .get_pixel_buffer()
                .map_err(|e| anyhow!("Failed to get pixel buffer for stride calculation: {}", e))?;
            let stride = pixel_buffer.get_bytes_per_row() as usize;
            let _guard = pixel_buffer
                .lock_mut()
                .map_err(|e| anyhow!("Failed to lock pixel buffer: {}", e))?;
            stride
        };

        info!("Determined screen stride: {}", stride);

        Ok(Self {
            frame_rx: rx,
            width: width as usize,
            height: height as usize,
            stride,
            _stream: Arc::new(Mutex::new(stream)),
        })
    }

    async fn capture_frame(&mut self) -> Result<(Vec<u8>, SystemTime)> {
        let sample = self.frame_rx.recv().await.ok_or_else(|| anyhow!("Frame channel closed"))?;
        let timestamp_value = SystemTime::now();

        let mut pixel_buffer = sample
            .get_pixel_buffer()
            .map_err(|e| anyhow!("Failed to get pixel buffer: {}", e))?;
        let guard = pixel_buffer
            .lock_mut()
            .map_err(|e| anyhow!("Failed to lock pixel buffer: {}", e))?;

        let expected_size = self.height * self.stride;
        let actual_size = guard.as_slice().len();
        if actual_size < expected_size {
            bail!(
                "Pixel buffer size ({}) is smaller than expected ({}), potential data corruption.",
                actual_size,
                expected_size
            );
        }

        Ok((guard.as_slice()[..expected_size].to_vec(), timestamp_value))
    }

    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = tracing_subscriber::filter::EnvFilter
        ::new("info")
        .add_directive("iroh_moq=debug".parse().unwrap());
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter).with_target(true).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    ffmpeg::init().expect("Failed to initialize FFmpeg");

    let mut rng = OsRng;
    let secret_key = SecretKey::generate(&mut rng);
    let node_id: NodeId = secret_key.public().into();
    println!("Publisher node ID: {}", node_id);
    println!("To connect, run the subscriber with: cargo run --example subscriber -- {}", node_id);

    println!("\n=================================================================");
    println!("SUBSCRIBER COMMAND:");
    println!("RUST_LOG=debug cargo run --example subscriber -- {} -o output.mp4", node_id);
    println!("\nTo enable video playback with FFplay, add the --play flag:");
    println!("RUST_LOG=debug cargo run --example subscriber -- {} -o output.mp4 --play", node_id);
    println!("=================================================================\n");

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

    info!("Initializing screen capture...");
    let mut capturer = ScreenCapturer::new().await?;
    let width = capturer.width() as u32;
    let height = capturer.height() as u32;
    info!("Capture dimensions: width={}, height={}, stride={}", width, height, capturer.stride);

    let video_config = VideoConfig {
        codec: "hevc".to_string(),
        mime_type: "video/mp4".to_string(),
        bitrate: 5_000_000,
        frame_rate: 30.0,
        keyframe_interval: 60,
    };

    let codec = encoder
        ::find_by_name("hevc_videotoolbox")
        .ok_or_else(|| anyhow!("HEVC VideoToolbox encoder not found"))?;
    let encoder_ctx = CodecContext::new_with_codec(codec);
    let mut encoder = encoder_ctx.encoder().video()?;

    encoder.set_width(width);
    encoder.set_height(height);
    encoder.set_format(FFmpegPixel::YUV420P);
    encoder.set_time_base((1, 90000));
    encoder.set_bit_rate(video_config.bitrate as usize);
    encoder.set_frame_rate(Some((video_config.frame_rate as i32, 1)));
    encoder.set_gop(video_config.keyframe_interval as u32);

    // Open the encoder
    let mut encoder = encoder.open_as(codec)?;

    // Create an empty init segment by default
    let mut init_segment = Vec::new();

    // Try encoding a dummy frame to generate headers
    info!("Sending dummy frame to generate initialization data...");
    let mut dummy_frame = FfmpegFrame::new(FFmpegPixel::YUV420P, width, height);
    dummy_frame.set_pts(Some(0));

    // Send the dummy frame to the encoder
    if let Err(e) = encoder.send_frame(&dummy_frame) {
        warn!("Failed to send dummy frame: {}. Init segment might be missing.", e);
    } else {
        // Receive packet(s) from the dummy frame
        let mut dummy_packet = packet::Packet::empty();
        match encoder.receive_packet(&mut dummy_packet) {
            Ok(_) => {
                // Success! Use this packet data as our init segment
                if let Some(data) = dummy_packet.data() {
                    info!("Using first encoded packet ({} bytes) as init segment", data.len());
                    init_segment = data.to_vec();
                }
            }
            Err(e) => warn!("Error getting init packet: {}", e),
        }
    }

    info!(
        "Init segment size: {} bytes. Data prefix: {:02X?}",
        init_segment.len(),
        &init_segment.get(..std::cmp::min(init_segment.len(), 64)).unwrap_or(&[])
    );
    if init_segment.is_empty() {
        warn!("Proceeding with empty init segment. Playback likely won't work.");
    }

    let init = MediaInit {
        codec: video_config.codec.clone(),
        mime_type: video_config.mime_type.clone(),
        width,
        height,
        frame_rate: video_config.frame_rate,
        bitrate: video_config.bitrate,
        init_segment,
    };

    let namespace = "/live/hevc_test".to_string();
    let (stream_id, chunk_sender) = client.publish_video_stream(namespace.clone(), init).await?;
    info!("Published video stream {} with namespace {}", stream_id, namespace);

    let mut scaler = Scaler::get(
        FFmpegPixel::BGRA,
        width,
        height,
        FFmpegPixel::YUV420P,
        width,
        height,
        Flags::BILINEAR | Flags::PRINT_INFO
    )?;
    info!("FFmpeg scaler initialized: BGRA -> YUV420P");

    let mut frame_count: u64 = 0;
    let start_time = SystemTime::now();
    let time_base = encoder.time_base();
    let frame_duration_ts = ((time_base.1 as f64) / (video_config.frame_rate as f64)) as i64;
    info!("Encoder timebase: {}/{}", time_base.0, time_base.1);
    info!("Calculated frame duration in timebase units: {}", frame_duration_ts);

    println!("Streaming... Press Ctrl+C to stop");
    let shutdown_token = CancellationToken::new();
    let shutdown_token_clone = shutdown_token.clone();

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.expect("Failed to install Ctrl+C handler");
        println!("\nCtrl+C received, shutting down gracefully...");
        shutdown_token_clone.cancel();
    });

    let mut last_stats_time = SystemTime::now();
    let mut frames_since_last_stats = 0;

    loop {
        tokio::select! {
            biased;

            _ = shutdown_token.cancelled() => {
                info!("Shutdown signal received, exiting streaming loop.");
                break;
            }

            frame_result = capturer.capture_frame() => {
                let (bgra_data, capture_timestamp) = match frame_result {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Failed to capture frame: {}", e);
                        if e.to_string().contains("Frame channel closed") {
                            info!("Capture channel closed, stopping.");
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };

                let mut bgra_frame = FfmpegFrame::new(FFmpegPixel::BGRA, width, height);
                let src_stride = capturer.stride;
                let dest_stride = bgra_frame.stride(0) as usize;
                let expected_bgra_len = height as usize * src_stride;

                if bgra_data.len() < expected_bgra_len {
                     error!(
                        "BGRA data too small ({} bytes) for dimensions ({}x{}, stride {} = {} bytes expected). Skipping frame.",
                        bgra_data.len(), width, height, src_stride, expected_bgra_len
                     );
                     continue;
                }

                for row in 0..height as usize {
                    let src_start = row * src_stride;
                    let src_end = src_start + width as usize * 4;
                    let dest_start = row * dest_stride;
                    let dest_end = dest_start + width as usize * 4;

                    if src_end > bgra_data.len() {
                         error!("Source frame bounds error during copy (row {}, src_end {}, data_len {}). Skipping frame.", row, src_end, bgra_data.len());
                         continue;
                    }
                     if dest_end > bgra_frame.data_mut(0).len() {
                         error!("Destination frame bounds error during copy (row {}, dest_end {}, data_len {}). Skipping frame.", row, dest_end, bgra_frame.data_mut(0).len());
                         continue;
                    }

                    bgra_frame.data_mut(0)[dest_start..dest_end].copy_from_slice(&bgra_data[src_start..src_end]);
                }

                let mut yuv_frame = FfmpegFrame::empty();
                if let Err(e) = scaler.run(&bgra_frame, &mut yuv_frame) {
                    error!("Failed to scale frame: {}", e);
                    continue;
                }

                let capture_ts_micros = capture_timestamp.duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros() as u64;
                let pts = frame_count as i64 * frame_duration_ts;
                yuv_frame.set_pts(Some(pts));

                if let Err(e) = encoder.send_frame(&yuv_frame) {
                    error!("Failed to send frame to encoder: {}", e);
                    continue;
                }

                let mut encoded_packet = packet::Packet::empty();
                loop {
                     match encoder.receive_packet(&mut encoded_packet) {
                         Ok(_) => {
                             if let Some(data) = encoded_packet.data() {
                                 let is_keyframe = encoded_packet.is_key();
                                 let packet_pts = encoded_packet.pts().unwrap_or(pts);
                                 let chunk_ts_micros = capture_ts_micros;

                                 let video_chunk = VideoChunk {
                                     timestamp: chunk_ts_micros,
                                     duration: (frame_duration_ts as f64 * time_base.0 as f64 / time_base.1 as f64 * 1_000_000.0) as u32,
                                     is_keyframe,
                                     dependency_sequence: None,
                                     data: data.to_vec(),
                                 };

                                 debug!(
                                     "Sending chunk: seq={}, ts={}, key={}, size={}",
                                     frame_count, chunk_ts_micros, is_keyframe, data.len()
                                 );

                                 if chunk_sender.is_closed() {
                                     info!("Chunk sender closed, likely subscriber disconnected.");
                                     shutdown_token.cancel();
                                     break;
                                 }
                                 if let Err(e) = chunk_sender.send(video_chunk).await {
                                     error!("Failed to send video chunk via Moq: {}", e);
                                     shutdown_token.cancel();
                                     break;
                                 }
                             }
                         }

                          Err(ffmpeg_next::Error::Eof) => {
                              info!("Encoder reached EOF during receive_packet.");
                              shutdown_token.cancel();
                              break;
                          }
                         Err(e) => {
                             error!("Failed to receive packet from encoder: {}", e);
                             shutdown_token.cancel();
                             break;
                         }
                     }
                     if shutdown_token.is_cancelled() { break; }
                }

                frame_count += 1;
                frames_since_last_stats += 1;

                if let Ok(elapsed) = SystemTime::now().duration_since(last_stats_time) {
                     if elapsed >= Duration::from_secs(2) {
                         let fps = (frames_since_last_stats as f64) / elapsed.as_secs_f64();
                         info!(
                             "Streaming stats: {:.2} fps ({} frames in {:.2}s), total frames: {}",
                             fps, frames_since_last_stats, elapsed.as_secs_f64(), frame_count
                         );
                         last_stats_time = SystemTime::now();
                         frames_since_last_stats = 0;
                    }
                }
            }
        }
        if shutdown_token.is_cancelled() {
            break;
        }
    }

    info!("Flushing encoder...");
    if let Err(e) = encoder.send_eof() {
        error!("Failed to send EOF to encoder: {}", e);
    } else {
        let mut encoded_packet = packet::Packet::empty();
        loop {
            match encoder.receive_packet(&mut encoded_packet) {
                Ok(_) => {
                    if let Some(data) = encoded_packet.data() {
                        let is_keyframe = encoded_packet.is_key();
                        let packet_pts = encoded_packet
                            .pts()
                            .unwrap_or((frame_count as i64) * frame_duration_ts);
                        let chunk_ts_micros = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_micros() as u64;
                        let video_chunk = VideoChunk {
                            timestamp: chunk_ts_micros,
                            duration: ((((frame_duration_ts as f64) * (time_base.0 as f64)) /
                                (time_base.1 as f64)) *
                                1_000_000.0) as u32,
                            is_keyframe,
                            dependency_sequence: None,
                            data: data.to_vec(),
                        };
                        if !chunk_sender.is_closed() {
                            let _ = chunk_sender.send(video_chunk).await;
                        }
                    }
                }
                Err(ffmpeg_next::Error::Eof) => {
                    info!("Encoder flushed completely.");
                    break;
                }
                Err(e) => {
                    error!("Error receiving flushed packet: {}", e);
                    break;
                }
            }
        }
    }

    let total_duration = SystemTime::now().duration_since(start_time).unwrap_or_default();
    info!(
        "Streaming finished. Total frames: {}. Duration: {:.2}s. Average FPS: {:.2}",
        frame_count,
        total_duration.as_secs_f64(),
        if total_duration.as_secs_f64() > 0.0 {
            (frame_count as f64) / total_duration.as_secs_f64()
        } else {
            0.0
        }
    );

    Ok(())
}
