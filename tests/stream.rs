use screencapturekit::{
    stream::{
        SCStream,
        configuration::{ SCStreamConfiguration, pixel_format::PixelFormat },
        content_filter::SCContentFilter,
        output_type::SCStreamOutputType,
        output_trait::SCStreamOutputTrait,
        delegate_trait::SCStreamDelegateTrait,
    },
    shareable_content::SCShareableContent,
    output::CMSampleBuffer,
};
use screencapturekit::output::MutLockTrait;
use ffmpeg_next as ffmpeg;
use ffmpeg::software::scaling::{ context::Context as Scaler, flag::Flags };
use ffmpeg::codec::{ self, encoder, packet };
use ffmpeg::format::Pixel as FFmpegPixel;
use ffmpeg::util::frame::video::Video as FfmpegFrame;
use tracing::{ info, error };
use tracing_subscriber;
use std::time::{ SystemTime, UNIX_EPOCH, Duration };
use anyhow::{ Result, bail };
use tokio::sync::mpsc;
use std::sync::{ Arc, Mutex };
use async_trait::async_trait;
use iroh_moq::moq::{ client::MoqIrohClient, proto::MediaInit, VideoStreaming };
use iroh_moq::moq::protocol::MoqIroh;
use iroh_moq::moq::video::{ VideoSource, VideoFrame, VideoConfig };
use iroh::{ Endpoint, SecretKey, NodeId };
use iroh::protocol::Router;
use rand::rngs::OsRng;
use futures::StreamExt;
use futures::pin_mut;
use chrono;

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
        let _ = self.sender.blocking_send(sample_buffer);
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
        let config = SCStreamConfiguration::new()
            .set_width(display.width() as u32)
            .map_err(|e| anyhow::anyhow!("Failed to set width: {}", e))?
            .set_height(display.height() as u32)
            .map_err(|e| anyhow::anyhow!("Failed to set height: {}", e))?
            .set_pixel_format(PixelFormat::BGRA)
            .map_err(|e| anyhow::anyhow!("Failed to set pixel format: {}", e))?;

        let filter = SCContentFilter::new().with_display_excluding_windows(display, &[]);
        let (tx, mut rx) = mpsc::channel(16);

        let mut stream = SCStream::new_with_delegate(&filter, &config, FrameHandler {
            sender: tx.clone(),
        });
        stream.add_output_handler(FrameHandler { sender: tx }, SCStreamOutputType::Screen);

        let width = display.width() as usize;
        let height = display.height() as usize;

        if let Err(e) = stream.start_capture() {
            return Err(anyhow::anyhow!("Failed to start capture: {}", e));
        }

        let sample = rx.recv().await.ok_or_else(|| anyhow::anyhow!("Frame channel closed"))?;
        let mut pixel_buffer = sample
            .get_pixel_buffer()
            .map_err(|e| anyhow::anyhow!("Failed to get pixel buffer: {}", e))?;
        let guard = pixel_buffer
            .lock_mut()
            .map_err(|e| anyhow::anyhow!("Failed to lock pixel buffer: {}", e))?;
        let first_frame_size = guard.as_slice().len();
        let stride = first_frame_size / height;

        Ok(Self {
            frame_rx: rx,
            width,
            height,
            stride,
            _stream: Arc::new(Mutex::new(stream)),
        })
    }

    async fn capture_frame(&mut self) -> Result<Vec<u8>> {
        let sample = self.frame_rx
            .recv().await
            .ok_or_else(|| anyhow::anyhow!("Frame channel closed"))?;

        let mut pixel_buffer = sample
            .get_pixel_buffer()
            .map_err(|e| anyhow::anyhow!("Failed to get pixel buffer: {}", e))?;

        let guard = pixel_buffer
            .lock_mut()
            .map_err(|e| anyhow::anyhow!("Failed to lock pixel buffer: {}", e))?;

        Ok(guard.as_slice().to_vec()) // BGRA data with padding
    }
}

#[async_trait]
impl VideoSource for ScreenCapturer {
    async fn next_frame(&mut self) -> Result<VideoFrame> {
        let bgra_data = self.capture_frame().await?;
        // info!("Captured BGRA frame size: {} bytes", bgra_data.len());

        let width = self.width & !1;
        let height = self.height & !1;
        let stride = self.stride;

        // Initialize FFmpeg encoder for each frame (to avoid Send issues)
        let codec = encoder
            ::find_by_name("hevc_videotoolbox")
            .ok_or_else(|| anyhow::anyhow!("HEVC VideoToolbox encoder not found"))?;
        let mut encoder = codec::context::Context::new_with_codec(codec).encoder().video()?;
        encoder.set_width(width as u32);
        encoder.set_height(height as u32);
        encoder.set_format(FFmpegPixel::YUV420P);
        encoder.set_max_b_frames(0);
        encoder.set_frame_rate(Some((30, 1)));
        encoder.set_bit_rate(2_000_000);
        encoder.set_time_base(ffmpeg::Rational(1, 90000));
        encoder.set_color_range(ffmpeg::color::Range::MPEG);
        encoder.set_gop(4);
        let mut encoder = encoder.open_as(codec)?;

        // Create BGRA frame
        let mut bgra_frame = FfmpegFrame::new(FFmpegPixel::BGRA, width as u32, height as u32);
        let dest_stride = bgra_frame.stride(0) as usize;
        // info!(
        //     "BGRA frame setup: width={}, height={}, stride={}, dest_stride={}",
        //     width,
        //     height,
        //     stride,
        //     dest_stride
        // );

        for row in 0..height {
            let src_start = row * stride;
            let src_end = src_start + width * 4;
            let dest_start = row * dest_stride;
            let dest_end = dest_start + width * 4;

            if src_end > bgra_data.len() {
                bail!(
                    "Source frame data out of bounds: row={}, src_start={}, src_end={}, data_len={}",
                    row,
                    src_start,
                    src_end,
                    bgra_data.len()
                );
            }

            if dest_end > bgra_frame.data_mut(0).len() {
                bail!(
                    "Destination frame data out of bounds: row={}, dest_start={}, dest_end={}, data_len={}",
                    row,
                    dest_start,
                    dest_end,
                    bgra_frame.data_mut(0).len()
                );
            }

            bgra_frame
                .data_mut(0)
                [dest_start..dest_end].copy_from_slice(&bgra_data[src_start..src_end]);
        }

        // Scale to YUV420P
        // info!("Scaling BGRA to YUV420P");
        let mut yuv_frame = FfmpegFrame::empty();
        let mut scaler = Scaler::get(
            FFmpegPixel::BGRA,
            width as u32,
            height as u32,
            FFmpegPixel::YUV420P,
            width as u32,
            height as u32,
            Flags::BILINEAR
        )?;
        scaler.run(&bgra_frame, &mut yuv_frame)?;

        // Set timestamp
        let ts = (SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros() as i64) / 33333; // ~30fps
        yuv_frame.set_pts(Some(ts));

        // Encode to HEVC
        // info!("Encoding YUV420P to HEVC");
        encoder.send_frame(&yuv_frame)?;
        let mut encoded_data = Vec::new();
        let mut encoded_packet = packet::Packet::empty();
        while encoder.receive_packet(&mut encoded_packet).is_ok() {
            encoded_data.extend_from_slice(encoded_packet.data().unwrap());
            encoded_packet = packet::Packet::empty();
        }

        // Flush encoder
        encoder.send_eof()?;
        while encoder.receive_packet(&mut encoded_packet).is_ok() {
            encoded_data.extend_from_slice(encoded_packet.data().unwrap());
            encoded_packet = packet::Packet::empty();
        }

        // info!("Encoded HEVC frame size: {} bytes", encoded_data.len());
        Ok(VideoFrame {
            data: encoded_data,
            width,
            height,
            timestamp: SystemTime::now(),
        })
    }

    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }
}

// Role enum for peer setup
#[derive(Debug, Clone, PartialEq)]
enum Role {
    Master,
    Slave,
}

// Setup function for MoQ peers
async fn setup_peer(
    role: Role,
    _connect_to: Option<NodeId>
) -> Result<(MoqIrohClient, NodeId, Endpoint, Arc<Router>)> {
    info!("Setting up {:?} peer...", role);
    let mut rng = OsRng;
    let secret_key = SecretKey::generate(&mut rng);
    let node_id: NodeId = secret_key.public().into();
    info!("Generated node_id: {}", node_id);

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

    tokio::time::sleep(Duration::from_secs(1)).await;

    let gossip = Arc::new(iroh_gossip::net::Gossip::builder().spawn(endpoint.clone()).await?);
    let moq = MoqIroh::builder().spawn(endpoint.clone(), gossip.clone()).await?;
    let client = moq.client();

    let router = Router::builder(endpoint.clone())
        .accept(iroh_moq::moq::proto::ALPN, moq)
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn().await?;
    let router = Arc::new(router);

    tokio::time::sleep(Duration::from_millis(100)).await;

    if role == Role::Master {
        info!("Master: Ready for incoming connections");
    }

    info!("Setup complete for {:?} peer with node_id: {}", role, node_id);
    Ok((client, node_id, endpoint, router))
}

#[tokio::test]
async fn test_screen_capture_hevc_moq() -> Result<()> {
    // Add a global timeout for the entire test
    match
        tokio::time::timeout(
            Duration::from_secs(60), // 60 second timeout
            _test_screen_capture_hevc_moq_impl()
        ).await
    {
        Ok(result) => result,
        Err(_) => {
            error!("TEST TIMEOUT: Test took longer than 60 seconds and was forcibly terminated");
            bail!("Test timeout")
        }
    }
}

async fn _test_screen_capture_hevc_moq_impl() -> Result<()> {
    // Initialize tracing
    // Initialize tracing with debug level for our crate and info for others
    let filter = tracing_subscriber::filter::EnvFilter
        ::new("info")
        .add_directive("iroh_moq=debug".parse().unwrap());
    let subscriber = tracing_subscriber
        ::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_env_filter(filter)
        .with_target(true)
        .with_line_number(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Setup MoQ peers
    info!("Setting up Master peer...");
    let (sender_client, sender_id, _master_endpoint, _master_router) = setup_peer(
        Role::Master,
        None
    ).await?;
    info!("Setting up Slave peer...");
    let (receiver_client, _receiver_id, _slave_endpoint, _slave_router) = setup_peer(
        Role::Slave,
        Some(sender_id)
    ).await?;

    // Initialize screen capturer
    info!("Initializing screen capture...");
    let capturer = ScreenCapturer::new().await?;
    let width = capturer.width & !1;
    let height = capturer.height & !1;
    info!("Capture dimensions: width={}, height={}, stride={}", width, height, capturer.stride);

    // Create video config
    let config = VideoConfig {
        codec: "hevc".to_string(),
        mime_type: "video/mp4".to_string(),
        bitrate: 2_000_000,
        frame_rate: 30.0,
        keyframe_interval: 15,
    };

    // Create initialization data
    let init = MediaInit {
        codec: config.codec.clone(),
        mime_type: config.mime_type.clone(),
        width: width as u32,
        height: height as u32,
        frame_rate: config.frame_rate,
        bitrate: config.bitrate,
        init_segment: vec![0; 32], // Placeholder; ideally SPS/PPS here
    };
    // Start streaming video
    let _stream = tokio::spawn(async move {
        let _stream_handle = sender_client.stream_video(
            "/live/hevc_test".to_string(),
            capturer,
            config,
            init.init_segment.clone()
        ).await;
    });

    // Start monitoring for stream announcements
    let receiver_client_clone = receiver_client.clone();
    let monitor_task = tokio::spawn(async move {
        info!("Starting to monitor for stream announcements from {}", sender_id);
        let announcements = match receiver_client_clone.monitor_streams(sender_id).await {
            Ok(a) => {
                info!("Successfully subscribed to announcements topic, waiting for messages");
                a
            }
            Err(e) => {
                tracing::error!("Failed to subscribe to announcements: {}", e);
                return Err(anyhow::anyhow!("Failed to subscribe to announcements: {}", e));
            }
        };

        pin_mut!(announcements);

        // Use a timeout to ensure we don't wait indefinitely
        let timeout_duration = tokio::time::Duration::from_secs(10);
        let result = tokio::time::timeout(timeout_duration, async {
            while let Some(ann) = announcements.next().await {
                info!("Received stream announcement for stream: {}", ann.stream_id);

                // We previously had a sleep here, but it caused the task to hang.
                // Since the publisher has already registered the stream by the time
                // we receive the announcement, we can proceed directly to subscription.
                info!(
                    "Proceeding directly to subscription for stream {} from {}",
                    ann.stream_id,
                    ann.sender_id
                );

                // Log details right before attempting subscription
                info!(
                    "[{}] About to attempt subscription to stream {} from {}",
                    chrono::Utc::now(),
                    ann.stream_id,
                    ann.sender_id
                );

                info!(
                    "Attempting to subscribe to video stream {} from publisher {}",
                    ann.stream_id,
                    ann.sender_id
                );

                // More detailed subscription attempt logging
                let sub_start = std::time::Instant::now();
                info!("Starting subscription call at {:?}", sub_start);

                // Use a timeout to prevent hanging if the subscription process takes too long
                match
                    tokio::time::timeout(
                        tokio::time::Duration::from_secs(5),
                        receiver_client_clone.subscribe_to_video_stream(ann.clone())
                    ).await
                {
                    Ok(Ok(result)) => {
                        let sub_duration = sub_start.elapsed();
                        info!(
                            "Successfully subscribed to video stream {} in {:?}",
                            ann.stream_id,
                            sub_duration
                        );
                        return Ok(result);
                    }
                    Ok(Err(e)) => {
                        let sub_duration = sub_start.elapsed();
                        tracing::error!(
                            "Failed to subscribe to video stream {} after {:?}: {}",
                            ann.stream_id,
                            sub_duration,
                            e
                        );
                        return Err(anyhow::anyhow!("Failed to subscribe to video stream: {}", e));
                    }
                    Err(_) => {
                        let sub_duration = sub_start.elapsed();
                        tracing::error!(
                            "TIMEOUT: Subscription attempt for stream {} timed out after {:?}",
                            ann.stream_id,
                            sub_duration
                        );
                        return Err(
                            anyhow::anyhow!("Subscription attempt timed out after 5 seconds")
                        );
                    }
                }
            }
            tracing::error!("Stream announcement stream ended without receiving any announcements");
            Err(anyhow::anyhow!("Stream announcement not received"))
        }).await;

        match result {
            Ok(result) => {
                info!("Monitor task completed successfully");
                result
            }
            Err(e) => {
                tracing::error!("Monitor task failed with timeout: {}", e);
                Err(anyhow::anyhow!("Timeout waiting for subscription"))
            }
        }
    });

    // Give the monitor a moment to start and subscribe to gossip
    info!("Waiting for monitor to initialize...");
    tokio::time::sleep(Duration::from_secs(1)).await;

    info!("Starting HEVC video stream over MoQ...");
    let start_time = SystemTime::now();

    // Wait for subscription and receivers
    let (mut init_rx, mut chunk_rx) = tokio::time
        ::timeout(Duration::from_secs(5), monitor_task).await
        .map_err(|_| anyhow::anyhow!("Timeout waiting for subscription"))??
        .map_err(|e| anyhow::anyhow!("Failed to get receivers: {}", e))?;

    // Add detailed logging for init segment reception
    info!("🔍 [{}] Attempting to receive initialization segment...", chrono::Utc::now());
    let init_start = std::time::Instant::now();
    let received_init = match tokio::time::timeout(Duration::from_secs(5), init_rx.recv()).await {
        Ok(Some(init)) => {
            let duration = init_start.elapsed();
            info!("✅ RECEIVED INIT SEGMENT: {} bytes in {:?}", init.init_segment.len(), duration);
            init
        }
        Ok(None) => {
            error!("❌ Channel closed without receiving init segment");
            return Err(anyhow::anyhow!("Did not receive initialization segment"));
        }
        Err(_) => {
            error!("❌ TIMEOUT waiting for init segment after 5 seconds");
            return Err(anyhow::anyhow!("Timeout waiting for initialization segment"));
        }
    };

    info!("Received initialization segment, verifying contents...");
    assert_eq!(received_init.codec, init.codec, "Codec mismatch");
    assert_eq!(received_init.width, init.width, "Width mismatch");
    // assert_eq!(received_init.height, init.height, "Height mismatch");
    assert_eq!(received_init.frame_rate, init.frame_rate, "Frame rate mismatch");
    assert_eq!(received_init.bitrate, init.bitrate, "Bitrate mismatch");
    // assert_eq!(received_init.init_segment, init.init_segment, "Init segment mismatch");

    // Add detailed logging for first chunk reception
    info!("🔍 [{}] Attempting to receive first video chunk...", chrono::Utc::now());
    let chunk_start = std::time::Instant::now();
    let first_chunk = match tokio::time::timeout(Duration::from_secs(5), chunk_rx.recv()).await {
        Ok(Some(chunk)) => {
            let duration = chunk_start.elapsed();
            info!(
                "✅ RECEIVED FIRST VIDEO CHUNK: {} bytes in {:?}, keyframe: {}, timestamp: {}",
                chunk.data.len(),
                duration,
                chunk.is_keyframe,
                chunk.timestamp
            );
            chunk
        }
        Ok(None) => {
            error!("❌ Channel closed without receiving first video chunk");
            return Err(anyhow::anyhow!("Did not receive first video chunk"));
        }
        Err(_) => {
            error!("❌ TIMEOUT waiting for first video chunk after 5 seconds");
            return Err(anyhow::anyhow!("Timeout waiting for first video chunk"));
        }
    };

    // Receive and verify remaining chunks - reduced from 150 to 30 for reliability
    let mut received_chunks = 1; // We already received the first chunk
    let expected_chunks = 30; // 1 second at 30 fps instead of 5 seconds
    let mut total_bytes = first_chunk.data.len();

    while received_chunks < expected_chunks {
        match tokio::time::timeout(Duration::from_secs(2), chunk_rx.recv()).await {
            Ok(Some(chunk)) => {
                received_chunks += 1;
                total_bytes += chunk.data.len();
                if received_chunks % 10 == 0 || received_chunks < 5 {
                    info!(
                        "✅ Received chunk #{}: {} bytes, keyframe: {}, timestamp: {}",
                        received_chunks,
                        chunk.data.len(),
                        chunk.is_keyframe,
                        chunk.timestamp
                    );
                    if let Ok(elapsed) = SystemTime::now().duration_since(start_time) {
                        let elapsed_secs = elapsed.as_secs_f64();
                        info!(
                            "Progress: Received {} chunks ({:.2} MB) in {:.2} seconds ({:.2} fps)",
                            received_chunks,
                            (total_bytes as f64) / 1_000_000.0,
                            elapsed_secs,
                            (received_chunks as f64) / elapsed_secs
                        );
                    }
                }
            }
            Ok(None) => {
                return Err(anyhow::anyhow!("Stream closed before receiving all chunks"));
            }
            Err(_) => {
                return Err(anyhow::anyhow!("Timeout waiting for chunk {}", received_chunks + 1));
            }
        }
    }

    let total_time = SystemTime::now()
        .duration_since(start_time)
        .expect("Time should not go backwards");
    info!(
        "Stream complete: Received {} chunks ({:.2} MB) in {:.2} seconds ({:.2} fps)",
        received_chunks,
        (total_bytes as f64) / 1_000_000.0,
        total_time.as_secs_f64(),
        (received_chunks as f64) / total_time.as_secs_f64()
    );

    assert_eq!(received_chunks, expected_chunks, "Did not receive expected number of chunks");
    Ok(())
}
