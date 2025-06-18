use iroh_moq::moq::proto::{
    MoqObject, serialize_object, deserialize_object, MEDIA_TYPE_VIDEO,
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use uuid::Uuid;

/// Simulated retransmission buffer with size limits
struct RetransmissionBuffer {
    max_size: usize,
    max_objects: usize,
    buffer: VecDeque<MoqObject>,
    total_bytes: usize,
}

impl RetransmissionBuffer {
    fn new(max_size: usize, max_objects: usize) -> Self {
        Self {
            max_size,
            max_objects,
            buffer: VecDeque::new(),
            total_bytes: 0,
        }
    }

    /// Add an object to the buffer, evicting old objects if necessary
    fn add(&mut self, object: MoqObject) -> Result<(), String> {
        let object_size = object.data.len() + object.name.len() + 32; // Approximate size
        
        // Check if single object is too large
        if object_size > self.max_size {
            return Err("Object too large for buffer".to_string());
        }
        
        // Evict old objects until there's space
        while self.buffer.len() >= self.max_objects || 
              (self.total_bytes + object_size > self.max_size && !self.buffer.is_empty()) {
            if let Some(old) = self.buffer.pop_front() {
                self.total_bytes -= old.data.len() + old.name.len() + 32;
            }
        }
        
        self.total_bytes += object_size;
        self.buffer.push_back(object);
        Ok(())
    }

    /// Find an object by sequence number
    fn find_by_sequence(&self, sequence: u64) -> Option<&MoqObject> {
        self.buffer.iter().find(|obj| obj.sequence == sequence)
    }

    /// Get buffer statistics
    fn stats(&self) -> (usize, usize) {
        (self.buffer.len(), self.total_bytes)
    }
}

#[tokio::test]
async fn test_retransmission_buffer_basic() {
    let mut buffer = RetransmissionBuffer::new(10_000, 10);
    
    // Add objects
    for i in 0..5 {
        let obj = MoqObject {
            name: format!("/test/{}", i),
            sequence: i,
            timestamp: 1000 + i,
            group_id: MEDIA_TYPE_VIDEO as u32,
            priority: 128,
            data: vec![0xFF; 1000],
        };
        
        assert!(buffer.add(obj).is_ok());
    }
    
    let (count, bytes) = buffer.stats();
    assert_eq!(count, 5);
    assert!(bytes > 5000);
    
    // Find specific object
    let found = buffer.find_by_sequence(3);
    assert!(found.is_some());
    assert_eq!(found.unwrap().sequence, 3);
    
    // Non-existent object
    let not_found = buffer.find_by_sequence(10);
    assert!(not_found.is_none());
}

#[tokio::test]
async fn test_retransmission_buffer_overflow_by_count() {
    let mut buffer = RetransmissionBuffer::new(100_000, 5); // Max 5 objects
    
    // Add more than max objects
    for i in 0..10 {
        let obj = MoqObject {
            name: format!("/test/{}", i),
            sequence: i,
            timestamp: 1000 + i,
            group_id: MEDIA_TYPE_VIDEO as u32,
            priority: 128,
            data: vec![0xFF; 100],
        };
        
        assert!(buffer.add(obj).is_ok());
    }
    
    let (count, _) = buffer.stats();
    assert_eq!(count, 5); // Should only have last 5 objects
    
    // Old objects should be evicted
    assert!(buffer.find_by_sequence(0).is_none());
    assert!(buffer.find_by_sequence(4).is_none());
    
    // Recent objects should be present
    assert!(buffer.find_by_sequence(5).is_some());
    assert!(buffer.find_by_sequence(9).is_some());
}

#[tokio::test]
async fn test_retransmission_buffer_overflow_by_size() {
    let mut buffer = RetransmissionBuffer::new(5_000, 100); // Max 5KB
    
    // Add objects that will exceed size limit
    for i in 0..10 {
        let obj = MoqObject {
            name: format!("/test/{}", i),
            sequence: i,
            timestamp: 1000 + i,
            group_id: MEDIA_TYPE_VIDEO as u32,
            priority: 128,
            data: vec![0xFF; 1000], // ~1KB each
        };
        
        assert!(buffer.add(obj).is_ok());
    }
    
    let (count, bytes) = buffer.stats();
    assert!(count < 10); // Should have evicted some
    assert!(bytes <= 5_000); // Should respect size limit
}

#[tokio::test]
async fn test_single_object_too_large() {
    let mut buffer = RetransmissionBuffer::new(1_000, 10); // Max 1KB
    
    let large_obj = MoqObject {
        name: "/test/large".to_string(),
        sequence: 1,
        timestamp: 1000,
        group_id: MEDIA_TYPE_VIDEO as u32,
        priority: 128,
        data: vec![0xFF; 2000], // 2KB - larger than buffer
    };
    
    let result = buffer.add(large_obj);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "Object too large for buffer");
}

#[tokio::test]
async fn test_priority_based_eviction() {
    // In a real implementation, we might want to evict based on priority
    let mut buffer = VecDeque::new();
    
    // Add mix of priorities
    for i in 0..10 {
        let obj = MoqObject {
            name: format!("/test/{}", i),
            sequence: i as u64,
            timestamp: 1000 + i as u64,
            group_id: MEDIA_TYPE_VIDEO as u32,
            priority: if i % 3 == 0 { 255 } else { 128 }, // Some high priority
            data: vec![0xFF; 100],
        };
        buffer.push_back(obj);
    }
    
    // Sort by priority (in real implementation, use a priority queue)
    let mut sorted: Vec<_> = buffer.into_iter().collect();
    sorted.sort_by_key(|obj| std::cmp::Reverse(obj.priority));
    
    // High priority objects should be first
    assert_eq!(sorted[0].priority, 255);
    assert_eq!(sorted[1].priority, 255);
}

#[tokio::test]
async fn test_concurrent_retransmission_requests() {
    use tokio::sync::{mpsc, RwLock};
    use std::sync::Arc;
    
    let buffer = Arc::new(RwLock::new(RetransmissionBuffer::new(100_000, 100)));
    
    // Fill buffer
    {
        let mut buf = buffer.write().await;
        for i in 0..50 {
            let obj = MoqObject {
                name: format!("/test/{}", i),
                sequence: i,
                timestamp: 1000 + i,
                group_id: MEDIA_TYPE_VIDEO as u32,
                priority: 128,
                data: vec![0xFF; 500],
            };
            buf.add(obj).unwrap();
        }
    }
    
    // Simulate concurrent retransmission requests
    let mut handles = vec![];
    
    for i in 0..10 {
        let buffer_clone = buffer.clone();
        let handle = tokio::spawn(async move {
            let sequence = (i * 5) as u64; // Request different sequences
            let start = Instant::now();
            
            let buf = buffer_clone.read().await;
            let found = buf.find_by_sequence(sequence);
            
            if let Some(obj) = found {
                // Simulate sending the object
                let serialized = serialize_object(obj);
                sleep(Duration::from_millis(10)).await;
                (true, start.elapsed(), serialized.len())
            } else {
                (false, start.elapsed(), 0)
            }
        });
        handles.push(handle);
    }
    
    // Wait for all requests
    let mut success_count = 0;
    let mut total_latency = Duration::ZERO;
    
    for handle in handles {
        let (found, latency, _size) = handle.await.unwrap();
        if found {
            success_count += 1;
            total_latency += latency;
        }
    }
    
    assert!(success_count > 0);
    let avg_latency = total_latency / success_count;
    println!("Average retransmission latency: {:?}", avg_latency);
}

#[tokio::test]
async fn test_retransmission_with_network_delay() {
    use tokio::sync::mpsc;
    
    let (tx, mut rx) = mpsc::channel::<(u64, mpsc::Sender<Option<MoqObject>>)>(10);
    
    // Simulate a retransmission server
    let server_handle = tokio::spawn(async move {
        let mut buffer = RetransmissionBuffer::new(100_000, 100);
        
        // Pre-fill with some objects
        for i in 0..20 {
            let obj = MoqObject {
                name: format!("/test/{}", i),
                sequence: i,
                timestamp: 1000 + i,
                group_id: MEDIA_TYPE_VIDEO as u32,
                priority: if i % 5 == 0 { 255 } else { 128 },
                data: vec![0xFF; 1000],
            };
            buffer.add(obj).unwrap();
        }
        
        // Handle retransmission requests
        while let Some((sequence, response_tx)) = rx.recv().await {
            // Simulate network delay
            sleep(Duration::from_millis(20)).await;
            
            let obj = buffer.find_by_sequence(sequence).cloned();
            let _ = response_tx.send(obj).await;
        }
    });
    
    // Client requests
    let mut client_handles = vec![];
    
    for i in [5, 10, 15, 25].iter() {
        let tx_clone = tx.clone();
        let sequence = *i;
        
        let handle = tokio::spawn(async move {
            let (response_tx, mut response_rx) = mpsc::channel(1);
            let start = Instant::now();
            
            // Request retransmission
            tx_clone.send((sequence, response_tx)).await.unwrap();
            
            // Wait for response
            let response = response_rx.recv().await.unwrap();
            let latency = start.elapsed();
            
            (sequence, response.is_some(), latency)
        });
        
        client_handles.push(handle);
    }
    
    // Collect results
    for handle in client_handles {
        let (seq, found, latency) = handle.await.unwrap();
        println!("Sequence {}: found={}, latency={:?}", seq, found, latency);
        
        if seq < 20 {
            assert!(found);
        } else {
            assert!(!found);
        }
    }
    
    drop(tx); // Signal server to stop
    let _ = server_handle.await;
}

#[tokio::test]
async fn test_sliding_window_buffer() {
    /// Sliding window buffer that maintains objects within a sequence range
    struct SlidingWindowBuffer {
        window_size: u64,
        min_sequence: u64,
        objects: VecDeque<MoqObject>,
    }
    
    impl SlidingWindowBuffer {
        fn new(window_size: u64) -> Self {
            Self {
                window_size,
                min_sequence: 0,
                objects: VecDeque::new(),
            }
        }
        
        fn add(&mut self, object: MoqObject) {
            // Update window bounds
            if object.sequence >= self.min_sequence + self.window_size {
                self.min_sequence = object.sequence - self.window_size + 1;
                
                // Remove objects outside window
                while let Some(front) = self.objects.front() {
                    if front.sequence < self.min_sequence {
                        self.objects.pop_front();
                    } else {
                        break;
                    }
                }
            }
            
            // Insert in order
            let pos = self.objects.binary_search_by_key(&object.sequence, |o| o.sequence)
                .unwrap_or_else(|pos| pos);
            self.objects.insert(pos, object);
        }
        
        fn get(&self, sequence: u64) -> Option<&MoqObject> {
            if sequence < self.min_sequence || sequence >= self.min_sequence + self.window_size {
                return None;
            }
            
            self.objects.binary_search_by_key(&sequence, |o| o.sequence)
                .ok()
                .and_then(|idx| self.objects.get(idx))
        }
    }
    
    let mut buffer = SlidingWindowBuffer::new(10);
    
    // Add objects
    for i in 0..20 {
        let obj = MoqObject {
            name: format!("/test/{}", i),
            sequence: i,
            timestamp: 1000 + i,
            group_id: MEDIA_TYPE_VIDEO as u32,
            priority: 128,
            data: vec![0xFF; 100],
        };
        buffer.add(obj);
    }
    
    // Objects within window should be available
    assert!(buffer.get(15).is_some());
    assert!(buffer.get(19).is_some());
    
    // Objects outside window should not be available
    assert!(buffer.get(5).is_none());
    assert!(buffer.get(9).is_none());
}

#[tokio::test]
async fn test_buffer_serialization_performance() {
    let mut buffer = Vec::new();
    
    // Add many objects to test serialization performance
    for i in 0..1000 {
        let obj = MoqObject {
            name: format!("/test/object/{}", i),
            sequence: i,
            timestamp: 1000000 + i,
            group_id: MEDIA_TYPE_VIDEO as u32,
            priority: if i % 10 == 0 { 255 } else { 128 },
            data: vec![(i & 0xFF) as u8; 1000],
        };
        buffer.push(obj);
    }
    
    let start = Instant::now();
    
    // Serialize all objects
    let mut total_size = 0;
    for obj in &buffer {
        let serialized = serialize_object(obj);
        total_size += serialized.len();
    }
    
    let elapsed = start.elapsed();
    let throughput = (total_size as f64) / elapsed.as_secs_f64() / 1_000_000.0; // MB/s
    
    println!("Serialized {} objects ({} MB) in {:?}", 
             buffer.len(), 
             total_size / 1_000_000,
             elapsed);
    println!("Throughput: {:.2} MB/s", throughput);
    
    assert!(throughput > 10.0); // Should achieve at least 10 MB/s
}