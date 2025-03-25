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
use std::time::{ SystemTime, UNIX_EPOCH };
use anyhow::{ Result, bail };
use tokio::sync::mpsc;
use std::sync::{ Arc, Mutex };
use async_trait::async_trait;
use iroh_moq::moq::VideoStreaming;
use iroh_moq::moq::protocol::MoqIroh;
use iroh_moq::moq::video::{ VideoSource, VideoFrame, VideoConfig };
use iroh::{ Endpoint, SecretKey, NodeId };
use iroh::protocol::Router;
use rand::rngs::OsRng;

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
        encoder.set_frame_rate(Some((30, 1)));
        encoder.set_bit_rate(2_000_000);
        encoder.set_time_base(ffmpeg::Rational(1, 90000));
        encoder.set_color_range(ffmpeg::color::Range::MPEG);
        encoder.set_gop(1);
        let mut encoder = encoder.open_as(codec)?;

        // Create BGRA frame
        let mut bgra_frame = FfmpegFrame::new(FFmpegPixel::BGRA, width as u32, height as u32);
        let dest_stride = bgra_frame.stride(0) as usize;

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

#[tokio::main]
async fn main() -> Result<()> {
    // Set up tracing
    let filter = tracing_subscriber::filter::EnvFilter
        ::new("info")
        .add_directive("iroh_moq=debug".parse().unwrap());
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter).with_target(true).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Set up publisher peer
    let mut rng = OsRng;
    let secret_key = SecretKey::generate(&mut rng);
    let node_id: NodeId = secret_key.public().into();
    println!("Publisher node ID: {}", node_id);
    println!("To connect, run the subscriber with: cargo run --example subscriber -- {}", node_id);

    // Print a more visible command that can be copied and pasted
    println!("\n=================================================================");
    println!("SUBSCRIBER COMMAND:");
    println!("RUST_LOG=debug cargo run --example subscriber -- {} -o output.mp4", node_id);
    println!("\nTo enable video playback with FFplay, add the --play flag:");
    println!("RUST_LOG=debug cargo run --example subscriber -- {} -o output.mp4 --play", node_id);
    println!("=================================================================\n");

    info!("Publisher node ID: {}", node_id);

    // Set up discovery
    let discovery = iroh::discovery::ConcurrentDiscovery::from_services(
        vec![
            Box::new(iroh::discovery::dns::DnsDiscovery::n0_dns()),
            Box::new(iroh::discovery::pkarr::PkarrPublisher::n0_dns(secret_key.clone()))
        ]
    );

    // Set up endpoint
    let endpoint = Endpoint::builder()
        .secret_key(secret_key.clone())
        .discovery(Box::new(discovery))
        .alpns(vec![iroh_moq::moq::proto::ALPN.to_vec(), iroh_gossip::ALPN.to_vec()])
        .bind().await?;

    // Set up gossip and MoQ
    let gossip = Arc::new(iroh_gossip::net::Gossip::builder().spawn(endpoint.clone()).await?);
    let moq = MoqIroh::builder().spawn(endpoint.clone(), gossip.clone()).await?;
    let client = moq.client();

    // Set up router
    let router = Router::builder(endpoint.clone())
        .accept(iroh_moq::moq::proto::ALPN, moq)
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn().await?;
    let _router = Arc::new(router);

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
    let init_segment = vec![0u8; 32]; // Placeholder; ideally SPS/PPS here

    // Print instructions
    println!("Publisher is ready to stream");

    // Start streaming video
    println!("Starting video stream...");
    let stream_handle = client.stream_video(
        "/live/hevc_test".to_string(),
        capturer,
        config,
        init_segment
    ).await?;

    // Keep streaming until user interrupts
    println!("Streaming... Press Ctrl+C to stop");

    // Wait for the streaming task to complete (or user interrupt)
    match stream_handle.await {
        Ok(result) => {
            if let Err(e) = result {
                error!("Streaming task error: {}", e);
            }
        }
        Err(e) => {
            error!("Failed to join streaming task: {}", e);
        }
    }

    Ok(())
}
