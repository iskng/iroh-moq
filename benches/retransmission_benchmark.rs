use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use iroh_moq::moq::proto::{MoqObject, MEDIA_TYPE_VIDEO};
use iroh_moq::moq::retransmission_buffer::{RetransmissionBuffer, RetransmissionBufferConfig};
use std::collections::VecDeque;

/// Benchmark the old VecDeque-based approach
fn bench_vecdeque_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("vecdeque_lookup");
    
    for size in [10, 50, 100, 200].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            // Create buffer with objects
            let mut buffer = VecDeque::new();
            for i in 0..size {
                let obj = MoqObject {
                    name: format!("/test/{}", i),
                    sequence: i as u64,
                    timestamp: 1000 + i as u64,
                    group_id: MEDIA_TYPE_VIDEO as u32,
                    priority: 128,
                    data: vec![0xFF; 1000],
                };
                buffer.push_back(obj);
            }
            
            b.iter(|| {
                // Linear search for middle element
                let target = size / 2;
                let found = buffer.iter().find(|o| o.sequence == target);
                black_box(found);
            });
        });
    }
    
    group.finish();
}

/// Benchmark the new RetransmissionBuffer approach
fn bench_retransmission_buffer_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("retransmission_buffer_lookup");
    
    for size in [10, 50, 100, 200].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            // Create buffer with objects
            let config = RetransmissionBufferConfig {
                max_objects: 300,
                max_bytes: 100 * 1024 * 1024,
                priority_eviction: true,
            };
            let mut buffer = RetransmissionBuffer::new(config);
            
            for i in 0..size {
                let obj = MoqObject {
                    name: format!("/test/{}", i),
                    sequence: i as u64,
                    timestamp: 1000 + i as u64,
                    group_id: MEDIA_TYPE_VIDEO as u32,
                    priority: 128,
                    data: vec![0xFF; 1000],
                };
                buffer.add(obj);
            }
            
            b.iter(|| {
                // O(log n) lookup for middle element
                let target = size / 2;
                let found = buffer.get(target);
                black_box(found);
            });
        });
    }
    
    group.finish();
}

/// Benchmark adding objects with eviction
fn bench_add_with_eviction(c: &mut Criterion) {
    let mut group = c.benchmark_group("add_with_eviction");
    
    // Test VecDeque approach
    group.bench_function("vecdeque", |b| {
        b.iter(|| {
            let mut buffer = VecDeque::with_capacity(100);
            
            for i in 0..200 {
                let obj = MoqObject {
                    name: format!("/test/{}", i),
                    sequence: i,
                    timestamp: 1000 + i,
                    group_id: MEDIA_TYPE_VIDEO as u32,
                    priority: (i % 256) as u8,
                    data: vec![0xFF; 1000],
                };
                
                buffer.push_back(obj);
                if buffer.len() > 100 {
                    buffer.pop_front();
                }
            }
            
            black_box(buffer.len());
        });
    });
    
    // Test RetransmissionBuffer with FIFO
    group.bench_function("retransmission_buffer_fifo", |b| {
        b.iter(|| {
            let config = RetransmissionBufferConfig {
                max_objects: 100,
                max_bytes: 100 * 1024 * 1024,
                priority_eviction: false,
            };
            let mut buffer = RetransmissionBuffer::new(config);
            
            for i in 0..200 {
                let obj = MoqObject {
                    name: format!("/test/{}", i),
                    sequence: i,
                    timestamp: 1000 + i,
                    group_id: MEDIA_TYPE_VIDEO as u32,
                    priority: (i % 256) as u8,
                    data: vec![0xFF; 1000],
                };
                
                buffer.add(obj);
            }
            
            black_box(buffer.stats().total_objects);
        });
    });
    
    // Test RetransmissionBuffer with priority eviction
    group.bench_function("retransmission_buffer_priority", |b| {
        b.iter(|| {
            let config = RetransmissionBufferConfig {
                max_objects: 100,
                max_bytes: 100 * 1024 * 1024,
                priority_eviction: true,
            };
            let mut buffer = RetransmissionBuffer::new(config);
            
            for i in 0..200 {
                let obj = MoqObject {
                    name: format!("/test/{}", i),
                    sequence: i,
                    timestamp: 1000 + i,
                    group_id: MEDIA_TYPE_VIDEO as u32,
                    priority: (i % 256) as u8,
                    data: vec![0xFF; 1000],
                };
                
                buffer.add(obj);
            }
            
            black_box(buffer.stats().total_objects);
        });
    });
    
    group.finish();
}

/// Benchmark range queries
fn bench_range_queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("range_queries");
    
    // Benchmark different range sizes
    for range_size in [10, 25, 50, 100].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("range_{}", range_size)), 
            range_size, 
            |b, &range_size| {
                // Setup buffer for each iteration group
                let config = RetransmissionBufferConfig {
                    max_objects: 200,
                    max_bytes: 100 * 1024 * 1024,
                    priority_eviction: false,
                };
                let mut buffer = RetransmissionBuffer::new(config);
                
                // Add 200 objects
                for i in 0..200 {
                    let obj = MoqObject {
                        name: format!("/test/{}", i),
                        sequence: i,
                        timestamp: 1000 + i,
                        group_id: MEDIA_TYPE_VIDEO as u32,
                        priority: 128,
                        data: vec![0xFF; 1000],
                    };
                    buffer.add(obj);
                }
                
                b.iter(|| {
                    let start = 50;
                    let end = start + range_size;
                    let objects = buffer.get_range(start, end);
                    black_box(objects.len());
                });
            }
        );
    }
    
    group.finish();
}

criterion_group!(
    benches,
    bench_vecdeque_lookup,
    bench_retransmission_buffer_lookup,
    bench_add_with_eviction,
    bench_range_queries
);

criterion_main!(benches);