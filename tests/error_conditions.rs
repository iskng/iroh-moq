use anyhow::Result;
use futures::StreamExt;
use iroh::{ Endpoint, SecretKey, protocol::Router };
use iroh_gossip::{net::Gossip, ALPN as GOSSIP_ALPN};
use iroh_moq::moq::{
    client::MoqIrohClient,
    proto::{ StreamAnnouncement, MediaInit },
    protocol::MoqIroh,
    video::{ VideoSource, VideoFrame, VideoConfig, VideoStreaming },
};
use std::{ sync::Arc, time::{ Duration, SystemTime } };
use tokio::{ sync::RwLock, time::{ sleep, timeout } };
use tracing::info;
use uuid::Uuid;
use rand::rngs::OsRng;

/// Test struct for simulating video source failures
struct FailingVideoSource {
    fail_after: usize,
    frames_sent: Arc<RwLock<usize>>,
    should_fail: bool,
}

impl FailingVideoSource {
    fn new(fail_after: usize) -> Self {
        Self {
            fail_after,
            frames_sent: Arc::new(RwLock::new(0)),
            should_fail: true,
        }
    }
}

#[async_trait::async_trait]
impl VideoSource for FailingVideoSource {
    async fn next_frame(&mut self) -> Result<VideoFrame> {
        if self.should_fail {
            let mut count = self.frames_sent.write().await;
            *count += 1;

            if *count > self.fail_after {
                return Err(anyhow::anyhow!("Simulated video source failure"));
            }
        }

        Ok(VideoFrame {
            data: vec![0xFF; 1920 * 1080 * 4], // BGRA format
            width: 1920,
            height: 1080,
            timestamp: SystemTime::now(),
        })
    }

    fn width(&self) -> usize {
        1920
    }

    fn height(&self) -> usize {
        1080
    }
}

async fn create_test_client() -> Result<(MoqIrohClient, Arc<Router>)> {
    let secret_key = SecretKey::generate(&mut OsRng);
    let endpoint = Endpoint::builder()
        .secret_key(secret_key.clone())
        .alpns(vec![iroh_moq::moq::proto::ALPN.to_vec(), GOSSIP_ALPN.to_vec()])
        .bind()
        .await?;

    let gossip = Arc::new(
        Gossip::builder()
            .spawn(endpoint.clone())
            .await?
    );

    let moq = MoqIroh::builder()
        .spawn(endpoint.clone(), gossip.clone())
        .await?;
    
    let client = moq.client();

    let router = Arc::new(
        Router::builder(endpoint.clone())
            .accept(iroh_moq::moq::proto::ALPN, moq)
            .accept(GOSSIP_ALPN, gossip.clone())
            .spawn()
            .await?
    );

    Ok((client, router))
}

#[tokio::test(flavor = "multi_thread")]
async fn test_publisher_video_source_failure() {
    let _ = tracing_subscriber::fmt::try_init();

    let (client, _router) = create_test_client().await.unwrap();
    let namespace = "/test/failing";

    // Create a failing video source
    let source = FailingVideoSource::new(5); // Fail after 5 frames

    // Start publishing - this should eventually fail
    let config = VideoConfig::default();
    let init_data = vec![0u8; 100]; // Fake init segment

    let stream_handle = client.stream_video(namespace.to_string(), source, config, init_data).await;

    match stream_handle {
        Ok(handle) => {
            // Wait for the streaming task to fail
            let result = timeout(Duration::from_secs(10), handle).await;
            info!("Stream result: {:?}", result);
        }
        Err(e) => {
            info!("Failed to start stream: {:?}", e);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_monitor_streams_timeout() {
    let _ = tracing_subscriber::fmt::try_init();

    let (client, _router) = create_test_client().await.unwrap();
    // Create a valid fake NodeId using a valid public key
    let fake_key = SecretKey::generate(&mut OsRng);
    let fake_sender = fake_key.public().into();

    // Monitor streams from non-existent sender
    let mut stream = client.monitor_streams(fake_sender).await.unwrap();

    // Should timeout waiting for announcements
    let result = timeout(Duration::from_secs(2), stream.next()).await;
    assert!(result.is_err(), "Should timeout waiting for announcements");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_subscribe_to_non_existent_stream() {
    let _ = tracing_subscriber::fmt::try_init();

    let (client, _router) = create_test_client().await.unwrap();

    // Create a fake announcement
    let announcement = StreamAnnouncement {
        stream_id: Uuid::new_v4(),
        sender_id: SecretKey::generate(&mut OsRng).public().into(),
        namespace: "/test/fake".to_string(),
        codec: "h264".to_string(),
        resolution: (1920, 1080),
        framerate: 30,
        bitrate: 5000000,
        timestamp: 0,
        relay_ids: vec![],
    };

    // Try to subscribe - should timeout
    let result = client.subscribe_to_video_stream(announcement).await;
    assert!(result.is_err(), "Subscribe to non-existent stream should fail");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_request_object_from_non_existent_publisher() {
    let _ = tracing_subscriber::fmt::try_init();

    let (client, _router) = create_test_client().await.unwrap();

    let stream_id = Uuid::new_v4();
    let namespace = "/test/retransmit";
    let fake_publisher = SecretKey::generate(&mut OsRng).public().into();

    // Request retransmission of a non-existent object
    let result = client.request_object(
        fake_publisher,
        stream_id,
        namespace.to_string(),
        42 // sequence number
    ).await;

    // Should fail or timeout
    assert!(result.is_err(), "Request from non-existent publisher should fail");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_terminate_non_existent_connection() {
    let _ = tracing_subscriber::fmt::try_init();

    let (client, _router) = create_test_client().await.unwrap();

    let fake_node = SecretKey::generate(&mut OsRng).public().into();

    // Try to terminate a non-existent connection
    let result = client.terminate_stream(fake_node, 0).await;

    // Should handle gracefully
    info!("Terminate result: {:?}", result);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_publish_with_empty_namespace() {
    let _ = tracing_subscriber::fmt::try_init();

    let (client, _router) = create_test_client().await.unwrap();

    // Create media init with empty namespace
    let init = MediaInit {
        codec: "h264".to_string(),
        mime_type: "video/mp4".to_string(),
        width: 1920,
        height: 1080,
        frame_rate: 30.0,
        bitrate: 5000000,
        init_segment: vec![0u8; 100],
    };

    // Try to publish with empty namespace
    let result = client.publish_video_stream("".to_string(), init).await;

    // Should handle empty namespace
    info!("Empty namespace publish result: {:?}", result);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_publish_attempts() {
    let _ = tracing_subscriber::fmt::try_init();

    let (client, _router) = create_test_client().await.unwrap();

    let namespace = "/test/concurrent";

    // Try to publish multiple streams concurrently
    let mut handles = vec![];

    for i in 0..5 {
        let client_clone = client.clone();
        let ns = format!("{}/{}", namespace, i);

        let handle = tokio::spawn(async move {
            let init = MediaInit {
                codec: "h264".to_string(),
                mime_type: "video/mp4".to_string(),
                width: 1920,
                height: 1080,
                frame_rate: 30.0,
                bitrate: 5000000,
                init_segment: vec![0u8; 100],
            };

            client_clone.publish_video_stream(ns, init).await
        });

        handles.push(handle);
    }

    // Wait for all attempts
    for handle in handles {
        match handle.await {
            Ok(result) => info!("Publish result: {:?}", result),
            Err(e) => info!("Task error: {:?}", e),
        }
    }
}

/// Mock video source that produces frames slowly
struct SlowVideoSource {
    frame_delay: Duration,
}

#[async_trait::async_trait]
impl VideoSource for SlowVideoSource {
    async fn next_frame(&mut self) -> Result<VideoFrame> {
        sleep(self.frame_delay).await;
        Ok(VideoFrame {
            data: vec![0x42; 640 * 480 * 4],
            width: 640,
            height: 480,
            timestamp: SystemTime::now(),
        })
    }

    fn width(&self) -> usize {
        640
    }

    fn height(&self) -> usize {
        480
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_slow_video_source() {
    let _ = tracing_subscriber::fmt::try_init();

    let (client, _router) = create_test_client().await.unwrap();

    let namespace = "/test/slow";
    let source = SlowVideoSource {
        frame_delay: Duration::from_secs(2), // Very slow frames
    };

    let config = VideoConfig {
        frame_rate: 30.0, // Expecting 30fps but source is slow
        ..Default::default()
    };

    let stream_result = client.stream_video(
        namespace.to_string(),
        source,
        config,
        vec![0u8; 100]
    ).await;

    match stream_result {
        Ok(handle) => {
            // Let it run for a bit
            sleep(Duration::from_secs(5)).await;
            handle.abort();
            info!("Slow source test completed");
        }
        Err(e) => {
            info!("Failed to start slow stream: {:?}", e);
        }
    }
}
