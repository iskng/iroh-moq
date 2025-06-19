# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

iroh-moq is a standalone protocol implementation for real-time media streaming over Iroh (a P2P networking library). It implements a P2P streaming protocol inspired by Media over QUIC (MoQ), providing real-time video/audio streaming capabilities with screen capture support.

## Build and Development Commands

### Building
```bash
cargo build                      # Build the library
cargo build --examples           # Build all examples
cargo build --features relay     # Build with relay support
cargo build --release           # Build optimized release version
```

### Testing
```bash
cargo test                                    # Run all tests
cargo test test_screen_capture_hevc_moq      # Run specific test
cargo test -- --nocapture                    # Run tests with output
cargo test --test stream                     # Run tests from specific file
cargo test --lib proto::                     # Run protocol unit tests
cargo test --test retransmission_tests       # Run retransmission tests
```

### Benchmarking
```bash
cargo bench --bench streaming_performance    # Run throughput benchmarks
cargo bench --bench latency_benchmark        # Run latency benchmarks
cargo bench --bench retransmission_benchmark # Run retransmission buffer benchmarks
cargo bench -- --sample-size 10              # Quick benchmark run
```

### Linting and Formatting
```bash
cargo clippy      # Run linter
cargo fmt         # Format code
```

### Running Examples
```bash
# Full demo with publisher and subscriber
./run_moq_demo.sh

# Individual components
cargo run --example publisher
cargo run --example subscriber -- <NODE_ID>
cargo run --example audio_publisher
cargo run --example audio_subscriber -- <NODE_ID>
cargo run --example http_publisher
cargo run --example http_subscriber -- <NODE_ID>
```

## Architecture Overview

### Core Components

1. **Protocol Layer** (`src/moq/protocol.rs`)
   - Implements MoqIroh protocol handler for QUIC streams
   - Stream types: Control (0x01), Data (0x02), HTTP (0x03)
   - ALPN: `iroh-moq/v1`

2. **Engine** (`src/moq/engine.rs`)
   - Core streaming engine managing publishers/subscribers
   - StreamActorState manages subscribers and buffering
   - Handles stream lifecycle and chunk distribution
   - Uses efficient RetransmissionBuffer with O(log n) lookup

3. **Client API** (`src/moq/client.rs`)
   - High-level client for publish/subscribe operations
   - Gossip-based stream discovery
   - Methods: `publish_video()`, `subscribe_video()`, etc.

4. **Protocol Messages** (`src/moq/proto.rs`)
   - Wire protocol definitions
   - Control messages: Subscribe, Unsubscribe, StreamHeader, StreamData
   - Stream announcements via gossip protocol

5. **Media Processing**
   - Video (`src/moq/video.rs`): Screen capture, HEVC/H.264 encoding
   - Audio (`src/moq/audio.rs`): Audio capture, Opus/G.722 encoding

6. **Retransmission Buffer** (`src/moq/retransmission_buffer.rs`)
   - Efficient buffer with O(log n) sequence lookup using BTreeMap
   - Priority-based eviction for quality of service
   - Configurable size limits (object count and bytes)
   - Special handling for init segments

### Key Design Patterns

- **Actor Pattern**: Each stream managed by independent actor with command channels
- **P2P Discovery**: Uses Iroh gossip for stream announcements
- **QUIC Streams**: Separate control and data streams per connection
- **Buffering**: Maintains recent chunks for late subscribers
- **Priority/QoS**: Objects have sequence numbers and priority levels

## Testing Approach

Tests use `#[tokio::test]` with timeout wrappers. Integration tests in `tests/stream.rs` verify end-to-end functionality with multiple peers (Master/Slave roles).

## Dependencies and Requirements

- FFmpeg with HEVC/H.264 support
- macOS: screencapturekit for screen capture
- Network: QUIC protocol with NAT traversal via Iroh
- Media: HEVC VideoToolbox encoder, Opus/G.722 audio codecs

## Feature Flags

- `relay`: Optional relay support (disabled by default)

## Testing Overview

The codebase includes comprehensive tests covering:

1. **Unit Tests** (`src/moq/proto_tests.rs`) - Protocol message serialization/deserialization
2. **Integration Tests** (`tests/stream.rs`) - End-to-end video streaming with real capture
3. **Retransmission Tests** (`tests/retransmission_tests.rs`) - Buffer management and overflow handling
4. **Performance Benchmarks** (`benches/`) - Throughput and latency measurements

Key test patterns:
- Async tests use `#[tokio::test]` with timeouts
- Integration tests verify multi-peer scenarios
- Benchmarks measure serialization, buffer operations, and concurrent processing