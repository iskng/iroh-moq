use crate::moq::proto::MoqObject;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use tracing::{debug, trace};

/// Configuration for the retransmission buffer
#[derive(Debug, Clone)]
pub struct RetransmissionBufferConfig {
    /// Maximum number of objects to keep in the buffer
    pub max_objects: usize,
    /// Maximum total size of objects in bytes
    pub max_bytes: usize,
    /// Whether to prioritize keeping high-priority objects
    pub priority_eviction: bool,
}

impl Default for RetransmissionBufferConfig {
    fn default() -> Self {
        Self {
            max_objects: 100,
            max_bytes: 50 * 1024 * 1024, // 50MB
            priority_eviction: true,
        }
    }
}

/// Statistics about the buffer
#[derive(Debug, Clone, Default)]
pub struct BufferStats {
    pub total_objects: usize,
    pub total_bytes: usize,
    pub evictions: u64,
    pub hits: u64,
    pub misses: u64,
}

/// Efficient retransmission buffer with O(log n) lookup
pub struct RetransmissionBuffer {
    /// Configuration
    config: RetransmissionBufferConfig,
    
    /// Index for fast sequence number lookup - O(log n)
    sequence_index: BTreeMap<u64, Arc<MoqObject>>,
    
    /// Queue maintaining insertion order for FIFO eviction
    insertion_order: VecDeque<u64>,
    
    /// Priority index for priority-based eviction (sequence -> priority)
    priority_index: BTreeMap<u8, Vec<u64>>,
    
    /// Current total size in bytes
    total_bytes: usize,
    
    /// Statistics
    stats: BufferStats,
    
    /// Optional init segment to always keep
    init_segment: Option<Arc<MoqObject>>,
}

impl RetransmissionBuffer {
    /// Create a new retransmission buffer with the given configuration
    pub fn new(config: RetransmissionBufferConfig) -> Self {
        Self {
            config,
            sequence_index: BTreeMap::new(),
            insertion_order: VecDeque::new(),
            priority_index: BTreeMap::new(),
            total_bytes: 0,
            stats: BufferStats::default(),
            init_segment: None,
        }
    }
    
    /// Add an object to the buffer
    pub fn add(&mut self, object: MoqObject) {
        let sequence = object.sequence;
        let priority = object.priority;
        let object_size = Self::calculate_object_size(&object);
        
        // Check if this is an init segment
        if object.group_id == crate::moq::proto::MEDIA_TYPE_INIT as u32 {
            debug!("Storing init segment for retransmission");
            self.init_segment = Some(Arc::new(object));
            return;
        }
        
        // Check if we already have this sequence
        if self.sequence_index.contains_key(&sequence) {
            trace!("Object with sequence {} already in buffer", sequence);
            return;
        }
        
        // Evict objects if necessary to make room
        self.evict_for_space(object_size);
        
        // Add to indices
        let object_arc = Arc::new(object);
        self.sequence_index.insert(sequence, object_arc.clone());
        self.insertion_order.push_back(sequence);
        
        // Update priority index
        self.priority_index
            .entry(priority)
            .or_insert_with(Vec::new)
            .push(sequence);
        
        // Update stats
        self.total_bytes += object_size;
        self.stats.total_objects = self.sequence_index.len();
        self.stats.total_bytes = self.total_bytes;
        
        trace!(
            "Added object seq={} to buffer (total: {} objects, {} bytes)",
            sequence,
            self.stats.total_objects,
            self.stats.total_bytes
        );
    }
    
    /// Get an object by sequence number - O(log n)
    pub fn get(&mut self, sequence: u64) -> Option<Arc<MoqObject>> {
        // Check init segment first
        if let Some(ref init) = self.init_segment {
            if init.sequence == sequence {
                self.stats.hits += 1;
                return Some(init.clone());
            }
        }
        
        match self.sequence_index.get(&sequence) {
            Some(object) => {
                self.stats.hits += 1;
                debug!("Buffer hit for sequence {}", sequence);
                Some(object.clone())
            }
            None => {
                self.stats.misses += 1;
                debug!("Buffer miss for sequence {}", sequence);
                None
            }
        }
    }
    
    /// Get objects in a sequence range
    pub fn get_range(&mut self, start_seq: u64, end_seq: u64) -> Vec<Arc<MoqObject>> {
        self.sequence_index
            .range(start_seq..=end_seq)
            .map(|(_, obj)| obj.clone())
            .collect()
    }
    
    /// Get buffer statistics
    pub fn stats(&self) -> &BufferStats {
        &self.stats
    }
    
    /// Clear the buffer
    pub fn clear(&mut self) {
        self.sequence_index.clear();
        self.insertion_order.clear();
        self.priority_index.clear();
        self.total_bytes = 0;
        self.stats.total_objects = 0;
        self.stats.total_bytes = 0;
        // Keep init segment
    }
    
    /// Evict objects to make room for a new object of the given size
    fn evict_for_space(&mut self, needed_bytes: usize) {
        // Check if we need to evict by count
        while self.sequence_index.len() >= self.config.max_objects {
            if self.config.priority_eviction {
                self.evict_lowest_priority();
            } else {
                self.evict_oldest();
            }
        }
        
        // Check if we need to evict by size
        while self.total_bytes + needed_bytes > self.config.max_bytes && !self.sequence_index.is_empty() {
            if self.config.priority_eviction {
                self.evict_lowest_priority();
            } else {
                self.evict_oldest();
            }
        }
    }
    
    /// Evict the oldest object (FIFO)
    fn evict_oldest(&mut self) {
        if let Some(sequence) = self.insertion_order.pop_front() {
            self.remove_object(sequence);
        }
    }
    
    /// Evict the lowest priority object
    fn evict_lowest_priority(&mut self) {
        // Find the lowest priority with objects
        let lowest_priority = self.priority_index.keys().next().copied();
        
        if let Some(priority) = lowest_priority {
            let sequence_to_remove = self.priority_index.get_mut(&priority)
                .and_then(|sequences| sequences.pop());
            
            if let Some(sequence) = sequence_to_remove {
                // Remove from insertion order
                self.insertion_order.retain(|&s| s != sequence);
                
                // Check if we need to clean up the priority entry
                let should_remove_priority = self.priority_index.get(&priority)
                    .map(|sequences| sequences.is_empty())
                    .unwrap_or(false);
                
                if should_remove_priority {
                    self.priority_index.remove(&priority);
                }
                
                // Remove the object
                self.remove_object(sequence);
            }
        } else {
            // Fallback to FIFO if no priority index
            self.evict_oldest();
        }
    }
    
    /// Remove an object from all indices
    fn remove_object(&mut self, sequence: u64) {
        if let Some(object) = self.sequence_index.remove(&sequence) {
            let object_size = Self::calculate_object_size(&object);
            self.total_bytes = self.total_bytes.saturating_sub(object_size);
            self.stats.evictions += 1;
            
            trace!(
                "Evicted object seq={} (size: {} bytes)",
                sequence,
                object_size
            );
        }
    }
    
    /// Calculate the approximate size of an object in bytes
    fn calculate_object_size(object: &MoqObject) -> usize {
        object.data.len() + 
        object.name.len() + 
        std::mem::size_of::<MoqObject>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moq::proto::MEDIA_TYPE_VIDEO;
    
    #[test]
    fn test_basic_operations() {
        let config = RetransmissionBufferConfig {
            max_objects: 5,
            max_bytes: 1000,
            priority_eviction: false,
        };
        
        let mut buffer = RetransmissionBuffer::new(config);
        
        // Add some objects
        for i in 0..5 {
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
        
        assert_eq!(buffer.stats().total_objects, 5);
        
        // Test retrieval
        let obj = buffer.get(2).unwrap();
        assert_eq!(obj.sequence, 2);
        assert_eq!(buffer.stats().hits, 1);
        
        // Test miss
        assert!(buffer.get(10).is_none());
        assert_eq!(buffer.stats().misses, 1);
        
        // Add another object, should evict the oldest
        let obj = MoqObject {
            name: "/test/6".to_string(),
            sequence: 6,
            timestamp: 1006,
            group_id: MEDIA_TYPE_VIDEO as u32,
            priority: 128,
            data: vec![0xFF; 100],
        };
        buffer.add(obj);
        
        assert_eq!(buffer.stats().total_objects, 5);
        assert_eq!(buffer.stats().evictions, 1);
        assert!(buffer.get(0).is_none()); // First object should be evicted
    }
    
    #[test]
    fn test_priority_eviction() {
        let config = RetransmissionBufferConfig {
            max_objects: 3,
            max_bytes: 1000,
            priority_eviction: true,
        };
        
        let mut buffer = RetransmissionBuffer::new(config);
        
        // Add objects with different priorities
        let priorities = [100, 255, 150];
        for (i, &priority) in priorities.iter().enumerate() {
            let obj = MoqObject {
                name: format!("/test/{}", i),
                sequence: i as u64,
                timestamp: 1000 + i as u64,
                group_id: MEDIA_TYPE_VIDEO as u32,
                priority,
                data: vec![0xFF; 100],
            };
            buffer.add(obj);
        }
        
        // Add another object, should evict lowest priority (100)
        let obj = MoqObject {
            name: "/test/3".to_string(),
            sequence: 3,
            timestamp: 1003,
            group_id: MEDIA_TYPE_VIDEO as u32,
            priority: 200,
            data: vec![0xFF; 100],
        };
        buffer.add(obj);
        
        assert!(buffer.get(0).is_none()); // Lowest priority object should be evicted
        assert!(buffer.get(1).is_some()); // High priority kept
        assert!(buffer.get(2).is_some()); // Medium priority kept
    }
    
    #[test]
    fn test_range_query() {
        let mut buffer = RetransmissionBuffer::new(Default::default());
        
        // Add objects
        for i in 0..10 {
            let obj = MoqObject {
                name: format!("/test/{}", i),
                sequence: i,
                timestamp: 1000 + i,
                group_id: MEDIA_TYPE_VIDEO as u32,
                priority: 128,
                data: vec![0xFF; 50],
            };
            buffer.add(obj);
        }
        
        // Get range
        let range = buffer.get_range(3, 7);
        assert_eq!(range.len(), 5);
        assert_eq!(range[0].sequence, 3);
        assert_eq!(range[4].sequence, 7);
    }
    
    #[test]
    fn test_init_segment_handling() {
        let mut buffer = RetransmissionBuffer::new(Default::default());
        
        // Add init segment
        let init = MoqObject {
            name: "/init".to_string(),
            sequence: 0,
            timestamp: 0,
            group_id: crate::moq::proto::MEDIA_TYPE_INIT as u32,
            priority: 255,
            data: vec![0xFF; 100],
        };
        buffer.add(init);
        
        // Init segment should always be retrievable
        assert!(buffer.get(0).is_some());
        
        // Clear buffer
        buffer.clear();
        
        // Init segment should still be there
        assert!(buffer.get(0).is_some());
    }
}