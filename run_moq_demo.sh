#!/bin/bash

# Function to clean up processes on exit
cleanup() {
  echo "Cleaning up processes..."
  
  # Kill the publisher process if it's running
  if [ -n "$PUBLISHER_PID" ] && kill -0 $PUBLISHER_PID 2>/dev/null; then
    kill $PUBLISHER_PID 2>/dev/null || true
  fi
  
  # Kill the subscriber process if it's running
  if [ -n "$SUBSCRIBER_PID" ] && kill -0 $SUBSCRIBER_PID 2>/dev/null; then
    kill $SUBSCRIBER_PID 2>/dev/null || true
  fi
  
  # Kill the ffplay process if it's running
  if [ -n "$FFPLAY_PID" ] && kill -0 $FFPLAY_PID 2>/dev/null; then
    kill $FFPLAY_PID 2>/dev/null || true
  fi
  
  exit
}

# Register cleanup function
trap cleanup SIGINT SIGTERM EXIT

# Set output file
OUTPUT_FILE="output.mp4"

# Allow overriding output file
if [ $# -ge 2 ] && [ "$1" = "-o" ]; then
  OUTPUT_FILE="$2"
  shift 2
fi

echo "Starting MOQ Demo"
echo "----------------"

# Build examples first to avoid compilation during the demo
cargo build --example publisher --example subscriber

# Start the publisher in the background
echo "Starting publisher..."
cargo run --example publisher > publisher_output.tmp 2>&1 &
PUBLISHER_PID=$!

# Wait for publisher to initialize and print its node ID
echo "Waiting for publisher to initialize..."
NODE_ID=""
MAX_ATTEMPTS=30
ATTEMPT=0

while [ -z "$NODE_ID" ] && [ $ATTEMPT -lt $MAX_ATTEMPTS ]; do
  ATTEMPT=$((ATTEMPT+1))
  
  # Check if publisher is still running
  if ! kill -0 $PUBLISHER_PID 2>/dev/null; then
    echo "Publisher failed to start."
    cat publisher_output.tmp
    rm -f publisher_output.tmp
    exit 1
  fi
  
  # Extract node ID from publisher output
  NODE_ID=$(grep -o "Publisher node ID: [a-f0-9]\{64\}" publisher_output.tmp | head -1 | cut -d' ' -f4)
  
  if [ -z "$NODE_ID" ]; then
    sleep 1
  fi
done

rm -f publisher_output.tmp

if [ -z "$NODE_ID" ]; then
  echo "Failed to get node ID from publisher."
  exit 1
fi

echo "Publisher running with Node ID: $NODE_ID"

# Remove existing output file if it exists
if [ -f "$OUTPUT_FILE" ]; then
  rm -f "$OUTPUT_FILE"
fi

# Start the subscriber WITHOUT auto-play
echo "Starting subscriber..."
cargo run --example subscriber -- $NODE_ID -o $OUTPUT_FILE &
SUBSCRIBER_PID=$!

# Wait for subscriber to initialize and create output file
echo "Waiting for subscriber to initialize..."
MAX_ATTEMPTS=10
ATTEMPT=0
while [ ! -f "$OUTPUT_FILE" ] && [ $ATTEMPT -lt $MAX_ATTEMPTS ]; do
  ATTEMPT=$((ATTEMPT+1))
  
  # Check if subscriber is still running
  if ! kill -0 $SUBSCRIBER_PID 2>/dev/null; then
    echo "Subscriber failed to start."
    exit 1
  fi
  
  sleep 1
  echo "Waiting for output file to be created ($ATTEMPT/$MAX_ATTEMPTS)..."
done

# Start ffplay with optimized parameters for low-latency streaming
if [ -f "$OUTPUT_FILE" ]; then
  echo "Starting video playback..."
 ffplay -loglevel warning -fflags nobuffer -flags low_delay -framedrop -infbuf -i "$OUTPUT_FILE" -vf "setpts=N/(30*TB)" -threads 4 -probesize 32 -analyzeduration 0 -sync ext -af aresample=async=1 -x 1280 -y 720 -window_title "MOQ-Iroh Low-Latency Stream" &
  FFPLAY_PID=$!
else
  echo "Error: Output file was not created. Cannot start playback."
  exit 1
fi

echo "MOQ demo running! Video should be playing."
echo "Press Ctrl+C to stop."

# Wait for publisher to exit or for user to press Ctrl+C
wait $PUBLISHER_PID 