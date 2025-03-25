use anyhow::{ Result, bail, anyhow };
use iroh::{ Endpoint, SecretKey, NodeId };
use iroh::protocol::Router;
use iroh_moq::moq::protocol::MoqIroh;
use iroh_moq::moq::proto::{ MediaInit, VideoChunk };
use rand::rngs::OsRng;
use std::env;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio;
use futures::{ StreamExt, pin_mut };
use tracing::{ info, error, debug, warn };
use tracing_subscriber;
use ffmpeg_next as ffmpeg;
use std::io::{ BufWriter, Write };
use std::fs::File;
use chrono::Local;
use std::process::{ Command, Stdio };
use std::time::{ SystemTime, UNIX_EPOCH };
use clap::{ Parser, ArgAction };

// Initialize FFmpeg
fn init_ffmpeg() -> Result<()> {
    ffmpeg::init().map_err(|e| anyhow!("Failed to initialize FFmpeg: {}", e))?;
    Ok(())
}

// Create MP4 muxer to save the video
struct VideoSaver {
    output_path: String,
    init_segment: Vec<u8>,
    _width: u32,
    _height: u32,
    _codec: String,
    file: Option<BufWriter<File>>,
    ffplay_command: Option<String>,
    chunk_count: usize,
    last_keyframe_time: Option<SystemTime>,
    ffplay_process: Option<std::process::Child>,
}

impl VideoSaver {
    fn new(output_path: &str, init: &MediaInit) -> Self {
        Self {
            output_path: output_path.to_string(),
            init_segment: init.init_segment.clone(),
            _width: init.width,
            _height: init.height,
            _codec: init.codec.clone(),
            file: None,
            ffplay_command: None,
            chunk_count: 0,
            last_keyframe_time: None,
            ffplay_process: None,
        }
    }

    // Create a named pipe and start FFplay for ultra-low-latency playback
    fn create_pipe_and_start_ffplay(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            // Remove existing file/pipe if it exists
            let _ = std::fs::remove_file(&self.output_path);

            // Create named pipe (FIFO)
            let fifo_status = std::process::Command::new("mkfifo").arg(&self.output_path).status()?;

            if !fifo_status.success() {
                bail!("Failed to create named pipe");
            }

            // Set permissive file permissions
            let mut perms = std::fs::metadata(&self.output_path)?.permissions();
            perms.set_mode(0o666);
            std::fs::set_permissions(&self.output_path, perms)?;

            info!("Created named pipe at {}", self.output_path);
        }

        #[cfg(not(unix))]
        {
            // On non-Unix platforms, we'll fall back to a regular file
            info!("Named pipes not supported on this platform, using regular file");
        }

        // Generate optimized FFplay command for lowest latency
        let ffplay_cmd = format!(
            "ffplay -loglevel warning -fflags nobuffer -flags low_delay -framedrop -infbuf -i {} \
             -vf \"setpts=N/(30*TB)\" -threads 4 -probesize 32 -analyzeduration 0 \
             -sync ext -af aresample=async=1 -x 1280 -y 720 -window_title \"MOQ-Iroh Low-Latency Stream\"",
            self.output_path
        );

        self.ffplay_command = Some(ffplay_cmd.clone());

        // Start FFplay in a separate process immediately to be ready for data
        info!("Starting FFplay for low-latency viewing...");
        let parts: Vec<&str> = ffplay_cmd.split_whitespace().collect();

        if parts.len() > 1 {
            let program = parts[0];
            let args: Vec<&str> = parts[1..].to_vec();

            // Check if ffplay is available
            if let Err(e) = std::process::Command::new(program).arg("-version").output() {
                warn!("FFplay not found or not executable: {}", e);
                warn!("Will continue with file output; you can manually run: {}", ffplay_cmd);
                return Ok(());
            }

            match
                Command::new(program).args(args).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
            {
                Ok(process) => {
                    info!("FFplay started successfully with PID: {}", process.id());
                    self.ffplay_process = Some(process);
                }
                Err(e) => {
                    warn!("Failed to start FFplay: {}", e);
                    warn!("Will continue with file output; you can manually run: {}", ffplay_cmd);
                }
            }
        }

        Ok(())
    }

    // Initialize file for streaming
    fn start_streaming(&mut self) -> Result<()> {
        // Only create pipe and start FFplay if auto_play is enabled
        if env::var("MOQ_AUTO_PLAY").unwrap_or_default() == "true" {
            if let Err(e) = self.create_pipe_and_start_ffplay() {
                warn!("Failed to create pipe: {}", e);
                warn!("Falling back to regular file output");
            }
        } else {
            info!("Auto-play disabled, not starting FFplay");
        }

        // Check if writing to file is disabled
        if env::var("MOQ_NO_FILE").unwrap_or_default() == "true" {
            info!("File output disabled, not writing to file");
            return Ok(());
        }

        // Now open the file/pipe for writing
        let file = match File::create(&self.output_path) {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to create output file: {}", e);
                warn!("Creating a new file with a timestamp suffix");
                let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
                let new_path = format!("{}.{}.mp4", self.output_path, timestamp);
                File::create(&new_path)?
            }
        };

        let mut writer = BufWriter::new(file);

        // Write initialization segment if available
        if !self.init_segment.is_empty() && self.init_segment.len() > 10 {
            writer.write_all(&self.init_segment)?;
            writer.flush()?;
        }

        self.file = Some(writer);

        // Create a shell script as backup for watching the stream
        let script_path = format!("{}_watch.sh", self.output_path);
        let mut script = File::create(&script_path)?;
        writeln!(script, "#!/bin/sh")?;
        writeln!(
            script,
            "{}",
            self.ffplay_command.as_ref().unwrap_or(&format!("ffplay {}", self.output_path))
        )?;

        // Make the script executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms)?;
        }

        println!("To watch the stream (if not already playing): sh {}", script_path);

        Ok(())
    }

    // Add a chunk in streaming mode - optimize for low latency
    fn add_chunk_streaming(&mut self, chunk: &VideoChunk) -> Result<()> {
        // Debug the chunk data

        if chunk.data.len() < 10 {
            warn!("Received very small chunk ({}), possible incomplete data", chunk.data.len());
            return Ok(());
        }

        // Write to file if one is open
        if let Some(writer) = &mut self.file {
            writer.write_all(&chunk.data)?;

            // Flush on keyframes to ensure timely display of new GOP
            if chunk.is_keyframe {
                writer.flush()?;
                self.last_keyframe_time = Some(SystemTime::now());
                info!("Flushed keyframe to output");
            } else {
                // For non-keyframes, flush less frequently to balance throughput with latency
                self.chunk_count += 1;
                if self.chunk_count % 15 == 0 {
                    // Reduced from 60 to 15 for lower latency
                    writer.flush()?;
                    debug!("Flushed {} non-keyframes to output", self.chunk_count);
                }
            }
        } else if env::var("MOQ_NO_FILE").unwrap_or_default() != "true" {
            // File is not open but we're supposed to be writing to a file
            warn!("No file open for writing. Call start_streaming() first.");
        }

        Ok(())
    }

    // Old method to add a chunk to memory (keeping for compatibility)
    #[allow(dead_code)]
    fn add_chunk(&mut self, chunk: VideoChunk) {
        // Now just a proxy to the streaming version to avoid storing chunks in memory
        let _ = self.add_chunk_streaming(&chunk);
    }

    // Write chunks directly to a file as HEVC

    #[allow(dead_code)]
    fn save_raw(&self) -> Result<()> {
        // If we're already streaming, no need to do anything
        if self.file.is_some() {
            info!("Video already saved to {}", self.output_path);
            return Ok(());
        }

        // Otherwise, create a new file and write the init segment
        let mut file = File::create(&self.output_path)?;

        // Write initialization segment if not empty
        if !self.init_segment.is_empty() && self.init_segment.len() > 10 {
            file.write_all(&self.init_segment)?;
        }

        info!("Saved raw HEVC data to {}", self.output_path);
        Ok(())
    }

    // Write chunks to an MP4 file using FFmpeg

    #[allow(dead_code)]
    fn save_mp4(&self) -> Result<()> {
        // If streaming, just close the file (flush happens automatically when dropped)
        if self.file.is_some() {
            // No action needed, file is already being written
        } else {
            // For non-streaming mode, use the original logic
            self.save_raw()?;
        }

        // Create a shell script to convert the raw file to MP4
        let script_path = format!("{}.sh", self.output_path);
        let mut script = File::create(&script_path)?;

        // Write the FFmpeg command to the script
        writeln!(script, "#!/bin/sh")?;
        writeln!(script, "ffmpeg -y -i {} -c:v copy {}.mp4", self.output_path, self.output_path)?;

        // Make the script executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms)?;
        }

        info!("Created conversion script at {}. Run this to create an MP4 file.", script_path);
        info!("To create the MP4, run: sh {}", script_path);

        Ok(())
    }

    // Start playing the video in a new process using FFplay
    fn start_playback(&self) -> Result<()> {
        if let Some(cmd) = &self.ffplay_command {
            // Start FFplay in a separate process
            let parts: Vec<&str> = cmd.split_whitespace().collect::<Vec<&str>>();

            if parts.len() > 1 {
                let program = parts[0];
                let args: Vec<&str> = parts[1..].to_vec();

                println!("Starting FFplay to view the stream...");
                match
                    Command::new(program)
                        .args(args)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                {
                    Ok(_) => {
                        println!(
                            "FFplay started. If no window appears, try running the command manually:"
                        );
                        println!("{}", cmd);
                        Ok(())
                    }
                    Err(e) => {
                        println!("Failed to start FFplay: {}", e);
                        println!("You can try running the command manually:");
                        println!("{}", cmd);
                        Err(anyhow!("Failed to start FFplay: {}", e))
                    }
                }
            } else {
                Err(anyhow!("Invalid FFplay command"))
            }
        } else {
            Err(anyhow!("No FFplay command available. Start streaming first."))
        }
    }

    fn stop(&mut self) -> Result<()> {
        // Flush and close the file
        if let Some(mut writer) = self.file.take() {
            info!("Flushing and closing output file...");
            if let Err(e) = writer.flush() {
                warn!("Error flushing file buffer: {}", e);
            }
            // The file will be closed when writer is dropped

            // Get file size for reporting
            if let Ok(metadata) = std::fs::metadata(&self.output_path) {
                let size_kb = metadata.len() / 1024;
                info!("Closed output file: {} ({} KB)", self.output_path, size_kb);
            } else {
                info!("Closed output file: {}", self.output_path);
            }
        }

        // Clean up FFplay process if running
        if let Some(mut process) = self.ffplay_process.take() {
            // First try to terminate gracefully
            match process.kill() {
                Ok(_) => {
                    info!("FFplay process terminated");
                    // Wait briefly for process to exit
                    let _ = process.wait();
                }
                Err(e) => {
                    warn!("Failed to terminate FFplay process: {}", e);
                    // Try force kill as a last resort
                    #[cfg(unix)]
                    {
                        let pid = process.id();
                        let _ = std::process::Command
                            ::new("kill")
                            .arg("-9")
                            .arg(pid.to_string())
                            .spawn();
                        info!("Sent SIGKILL to FFplay process");
                    }
                }
            }
        }

        info!("Video saver stopped");
        Ok(())
    }
}

// Implement Drop for VideoSaver to ensure cleanup on exit
impl Drop for VideoSaver {
    fn drop(&mut self) {
        // Make sure to close the file and kill FFplay when the VideoSaver is dropped
        if self.file.is_some() || self.ffplay_process.is_some() {
            if let Err(e) = self.stop() {
                eprintln!("Error during VideoSaver cleanup: {}", e);
            }
        }
    }
}
#[derive(Clone, Debug)]
struct TimingInfo {
    _frame_timestamp: u64,
    _received_time: SystemTime,
    _displayed_time: Option<SystemTime>,
    _is_keyframe: bool,
    _sequence: u64,
}

#[derive(Parser, Debug)]
#[clap(author, version, about = "MOQ-Iroh video subscriber", long_about = None)]
struct Args {
    /// Publisher node ID to connect to
    #[clap(index = 1)]
    publisher_id: String,

    /// Output file path for the video stream (if not specified, no file will be created)
    #[clap(short, long)]
    output: Option<String>,

    /// Automatically play the video stream using FFplay
    #[clap(long, action = ArgAction::SetTrue)]
    play: bool,

    /// Disable automatic video playback (overrides --play)
    #[clap(long, action = ArgAction::SetTrue)]
    no_play: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing for better logging
    let filter = tracing_subscriber::filter::EnvFilter
        ::new("info")
        .add_directive("iroh_moq=debug".parse().unwrap());
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter).with_target(true).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Initialize FFmpeg
    init_ffmpeg()?;

    // Parse command-line arguments
    let args = Args::parse();

    // Default to no auto-play unless --play is explicitly set (and not overridden by --no-play)
    let auto_play = args.play && !args.no_play;

    // Set environment variable for VideoSaver to check
    if auto_play {
        std::env::set_var("MOQ_AUTO_PLAY", "true");
    } else {
        std::env::set_var("MOQ_AUTO_PLAY", "false");
    }

    // Determine if file output should be enabled (only if output path is specified)
    let file_output_enabled = args.output.is_some();

    // Set environment variable for file output control
    if file_output_enabled {
        std::env::set_var("MOQ_NO_FILE", "false");
        info!("File output enabled, writing to: {}", args.output.as_ref().unwrap());
    } else {
        std::env::set_var("MOQ_NO_FILE", "true");
        info!("File output is disabled");
    }

    info!("Auto-play is {}", if auto_play { "enabled" } else { "disabled" });

    // Parse publisher node ID
    let publisher_id = NodeId::from_str(&args.publisher_id).map_err(|_|
        anyhow!("Invalid node ID format")
    )?;

    // Setup subscriber peer
    info!("Setting up subscriber peer...");
    let mut rng = OsRng;
    let secret_key = SecretKey::generate(&mut rng);
    let public_key = secret_key.public();
    let node_id: NodeId = public_key.into();
    info!("Subscriber node ID: {}", node_id);

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
    let _router = Router::builder(endpoint.clone())
        .accept(iroh_moq::moq::proto::ALPN, moq.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn().await?;
    let _router = Arc::new(_router);

    // Set up a channel for graceful shutdown
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Set up Ctrl+C handler for graceful shutdown, but only if we need to handle
    // file operations or FFplay process
    if file_output_enabled || auto_play {
        let shutdown_tx_clone = shutdown_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = tokio::signal::ctrl_c().await {
                error!("Failed to listen for Ctrl+C: {}", e);
                return;
            }
            info!("Received Ctrl+C, initiating graceful shutdown...");
            let _ = shutdown_tx_clone.send(()).await;

            // Add a timeout to force exit if graceful shutdown takes too long
            tokio::time::sleep(Duration::from_secs(5)).await;
            info!("Shutdown timeout reached, forcing exit");
            std::process::exit(0);
        });
        info!("Ctrl+C handler installed for graceful shutdown");
    } else {
        info!("No file operations or playback active, using default Ctrl+C behavior");
    }

    info!("Connecting to publisher: {}", publisher_id);

    // Start monitoring for stream announcements
    info!("Waiting for stream announcements from publisher...");
    let announcements = client.monitor_streams(publisher_id).await?;
    pin_mut!(announcements);

    // Create a default MediaInit to initialize the VideoSaver
    let default_init = MediaInit {
        codec: "avc1.64001f".to_string(),
        mime_type: "video/mp4".to_string(),
        width: 1280,
        height: 720,
        frame_rate: 30.0,
        bitrate: 5_000_000,
        init_segment: Vec::new(),
    };

    // Initialize video saver with the provided output path or a temporary path
    let output_path = args.output.as_deref().unwrap_or("temp_path_not_used.mp4");
    let mut video_saver = VideoSaver::new(output_path, &default_init);

    // Process incoming announcements
    loop {
        tokio::select! {
            // Check for shutdown signal
            _ = shutdown_rx.recv() => {
                info!("Shutting down...");
                // Force close all resources
                if let Some(mut process) = video_saver.ffplay_process.take() {
                    let _ = process.kill();
                }
                video_saver.stop()?;
                info!("Gracefully shut down, exiting now");
                break;
            }
            
            // Process stream announcements
            announcement = announcements.next() => {
                match announcement {
                    Some(announcement) => {
                        info!("Received stream announcement: {:?}", announcement);

                        // Subscribe to the announced stream
                        let result = client.subscribe_to_video_stream(announcement.clone()).await;

                        let (mut init_rx, mut chunk_rx) = match result {
                            Ok((init, video)) => {
                                info!("Successfully subscribed to stream");
                                (init, video)
                            }
                            Err(e) => {
                                error!("Failed to subscribe to stream: {}", e);
                                continue;
                            }
                        };

                        // Process initialization segments
                        if let Some(init) = init_rx.recv().await {
                            info!("Received initialization segment: {} bytes", init.init_segment.len());
                            
                            // Log the first few bytes of the init segment to debug
                            if !init.init_segment.is_empty() {
                                let prefix = if init.init_segment.len() >= 20 {
                                    format!("{:02X?}", &init.init_segment[0..20])
                                } else {
                                    format!("{:02X?}", &init.init_segment)
                                };
                                info!("Init segment header: {}", prefix);
                            } else {
                                warn!("Init segment is empty, video playback may fail");
                            }

                            // Create a new video saver with the received initialization
                            video_saver = VideoSaver::new(output_path, &init);
                            if let Err(e) = video_saver.start_streaming() {
                                error!("Failed to start video streaming: {}", e);
                                
                                // Only retry with a different path if file output is enabled
                                if file_output_enabled {
                                    let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
                                    let new_path = format!("{}.{}.mp4", output_path, timestamp);
                                    warn!("Retrying with alternative path: {}", new_path);
                                    
                                    video_saver = VideoSaver::new(&new_path, &init);
                                    if let Err(e) = video_saver.start_streaming() {
                                        error!("Failed again to start video streaming: {}", e);
                                        let _ = shutdown_tx.send(()).await;
                                        continue;
                                    }
                                } else {
                                    // If no file output is enabled, we can continue without a file
                                    warn!("Continuing without file output");
                                }
                            }
                            
                            // Auto-start playback if enabled
                            if env::var("MOQ_AUTO_PLAY").unwrap_or_default() == "true" {
                                info!("Auto-play enabled, attempting to start FFplay...");
                                if let Err(e) = video_saver.start_playback() {
                                    warn!("Failed to auto-start playback: {}", e);
                                }
                            } else {
                                info!("Auto-play disabled, not starting FFplay");
                            }
                        } else {
                            warn!("No initialization segment received, continuing with default settings");
                            
                            // Try to start streaming with default settings
                            if let Err(e) = video_saver.start_streaming() {
                                error!("Failed to start video streaming: {}", e);
                                
                                // Only retry with a different path if file output is enabled
                                if file_output_enabled {
                                    let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
                                    let new_path = format!("{}.{}.mp4", output_path, timestamp);
                                    warn!("Retrying with alternative path: {}", new_path);
                                    
                                    video_saver = VideoSaver::new(&new_path, &default_init);
                                    if let Err(e) = video_saver.start_streaming() {
                                        error!("Failed again to start video streaming: {}", e);
                                        let _ = shutdown_tx.send(()).await;
                                        continue;
                                    }
                                } else {
                                    // If no file output is enabled, we can continue without a file
                                    warn!("Continuing without file output");
                                }
                            }
                        }

                        // Create metrics for tracking performance
                        let mut chunk_count = 0;
                        let mut total_bytes = 0;
                        let start_time = SystemTime::now();
                        let mut last_stats_time = start_time;
                        let mut timing_info = Vec::new();
                        let mut min_latency = Duration::from_secs(3600);
                        let mut max_latency = Duration::from_secs(0);
                        let mut total_latency = Duration::from_secs(0);
                        
                        // Add a watchdog timer to detect if chunks stop flowing
                        let mut last_chunk_time = SystemTime::now();
                        let chunk_timeout = Duration::from_secs(5);

                        // Process video chunks with optimized latency tracking
                        let mut chunk_stream_done = false;
                        while !chunk_stream_done {
                            tokio::select! {
                                // Check for shutdown signal
                                _ = shutdown_rx.recv() => {
                                    info!("Shutting down during stream...");
                                    // Ensure FFplay is killed
                                    if let Some(mut process) = video_saver.ffplay_process.take() {
                                        let _ = process.kill();
                                    }
                                    chunk_stream_done = true;
                                }
                                
                                // Check for chunk timeout
                                _ = tokio::time::sleep(Duration::from_millis(1000)) => {
                                    let now = SystemTime::now();
                                    if now.duration_since(last_chunk_time).unwrap_or_default() > chunk_timeout {
                                        warn!("No chunks received for {} seconds, possible connection issue", 
                                             chunk_timeout.as_secs());
                                        
                                        // Try to check if FFplay is still running
                                        if let Some(ref mut process) = video_saver.ffplay_process {
                                            // Try to kill the process if we're in shutdown mode
                                            if chunk_stream_done {
                                                let _ = process.kill();
                                                continue;
                                            }
                                            
                                            match process.try_wait() {
                                                Ok(Some(status)) => {
                                                    warn!("FFplay has exited with status: {}", status);
                                                },
                                                Ok(None) => {
                                                    info!("FFplay is still running with PID: {}", process.id());
                                                },
                                                Err(e) => {
                                                    warn!("Failed to check FFplay status: {}", e);
                                                }
                                            }
                                        }
                                        
                                        // Print current stats to provide some feedback
                                        if chunk_count > 0 {
                                            let elapsed = now.duration_since(start_time).unwrap_or_default();
                                            let fps = (chunk_count as f64) / elapsed.as_secs_f64();
                                            let mbytes = (total_bytes as f64) / (1024.0 * 1024.0);
                                            let avg_latency = if chunk_count > 0 {
                                                (total_latency.as_secs_f64() * 1000.0) / (chunk_count as f64)
                                            } else {
                                                0.0
                                            };
                                            
                                            println!("Current stream stats (waiting for more chunks):");
                                            println!(
                                                "Received {} chunks ({:.2} MB) in {:.2} seconds ({:.2} fps)",
                                                chunk_count,
                                                mbytes,
                                                elapsed.as_secs_f64(),
                                                fps
                                            );
                                            
                                            if chunk_count > 1 {
                                                println!(
                                                    "LATENCY METRICS: avg={:.2} ms, min={:.2} ms, max={:.2} ms",
                                                    avg_latency,
                                                    min_latency.as_secs_f64() * 1000.0,
                                                    max_latency.as_secs_f64() * 1000.0
                                                );
                                            }
                                        }
                                    }
                                }
                                
                                // Process video chunks
                                chunk = chunk_rx.recv() => {
                                    match chunk {
                                        Some(chunk) => {
                                            // Update chunk received time
                                            last_chunk_time = SystemTime::now();
                                            
                                            // Calculate latency for this chunk
                                            let now = SystemTime::now();
                                            let chunk_time = UNIX_EPOCH + Duration::from_micros(chunk.timestamp);
                                            let latency = match now.duration_since(chunk_time) {
                                                Ok(duration) => duration,
                                                Err(_) => {
                                                    warn!("Chunk timestamp is in the future, possible clock sync issue");
                                                    Duration::from_secs(0)
                                                }
                                            };

                                            // Update latency statistics
                                            if latency < min_latency {
                                                min_latency = latency;
                                            }
                                            if latency > max_latency {
                                                max_latency = latency;
                                            }
                                            total_latency += latency;

                                            // Record detailed timing info
                                            timing_info.push(TimingInfo {
                                                _frame_timestamp: chunk.timestamp,
                                                _received_time: now,
                                                _displayed_time: None,
                                                _is_keyframe: chunk.is_keyframe,
                                                _sequence: chunk_count as u64,
                                            });

                                        

                                            // Process the chunk
                                            if let Err(e) = video_saver.add_chunk_streaming(&chunk) {
                                                error!("Failed to process video chunk: {}", e);
                                            }
                                            
                                            chunk_count += 1;
                                            total_bytes += chunk.data.len();

                                            // Print statistics every second
                                            if now.duration_since(last_stats_time).unwrap_or_default() >= Duration::from_secs(1) {
                                                let elapsed = now.duration_since(start_time).unwrap_or_default();
                                                let fps = (chunk_count as f64) / elapsed.as_secs_f64();
                                                let mbytes = (total_bytes as f64) / (1024.0 * 1024.0);

                                                let avg_latency = if chunk_count > 0 {
                                                    (total_latency.as_secs_f64() * 1000.0) / (chunk_count as f64)
                                                } else {
                                                    0.0
                                                };

                                                println!(
                                                    "Received {} chunks ({:.2} MB) in {:.2} seconds ({:.2} fps)",
                                                    chunk_count,
                                                    mbytes,
                                                    elapsed.as_secs_f64(),
                                                    fps
                                                );

                                                println!(
                                                    "LATENCY METRICS: avg={:.2} ms, min={:.2} ms, max={:.2} ms",
                                                    avg_latency,
                                                    min_latency.as_secs_f64() * 1000.0,
                                                    max_latency.as_secs_f64() * 1000.0
                                                );

                                                last_stats_time = now;
                                            }
                                        }
                                        None => {
                                            info!("Video stream ended");
                                            chunk_stream_done = true;
                                        }
                                    }
                                }
                            }
                        }

                        // Print final statistics
                        let elapsed = SystemTime::now().duration_since(start_time).unwrap_or_default();
                        let fps = (chunk_count as f64) / elapsed.as_secs_f64();
                        let mbytes = (total_bytes as f64) / (1024.0 * 1024.0);
                        let avg_latency = if chunk_count > 0 {
                            (total_latency.as_secs_f64() * 1000.0) / (chunk_count as f64)
                        } else {
                            0.0
                        };

                        println!("Stream finished:");
                        println!(
                            "Received {} chunks ({:.2} MB) in {:.2} seconds ({:.2} fps)",
                            chunk_count,
                            mbytes,
                            elapsed.as_secs_f64(),
                            fps
                        );

                        println!(
                            "LATENCY METRICS: avg={:.2} ms, min={:.2} ms, max={:.2} ms",
                            avg_latency,
                            min_latency.as_secs_f64() * 1000.0,
                            max_latency.as_secs_f64() * 1000.0
                        );

                        // Clean up
                        video_saver.stop()?;
                    }
                    None => {
                        info!("Stream announcements channel closed");
                        break;
                    }
                }
            }
        }
    }

    info!("Subscriber exiting");
    Ok(())
}
