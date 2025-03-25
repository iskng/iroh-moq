#!/bin/bash

# Function to clean up processes on exit
cleanup() {
  echo "Cleaning up..."
  
  # Kill the publisher process if it's running
  if [ -n "$PUBLISHER_PID" ] && kill -0 $PUBLISHER_PID 2>/dev/null; then
    echo "Killing publisher process $PUBLISHER_PID"
    kill $PUBLISHER_PID 2>/dev/null || true
  fi
  
  # Kill the subscriber process if it's running - note that it might be in a different terminal
  if [ -n "$SUBSCRIBER_PID" ] && [ -f "logs/subscriber_pid.txt" ]; then
    SUBSCRIBER_PID=$(cat logs/subscriber_pid.txt)
    if [ -n "$SUBSCRIBER_PID" ] && kill -0 $SUBSCRIBER_PID 2>/dev/null; then
      echo "Killing subscriber process $SUBSCRIBER_PID"
      kill $SUBSCRIBER_PID 2>/dev/null || true
    fi
  fi
  
  # Show final output.mp4 info if it exists
  if [ -f "$OUTPUT_FILE" ] && [ -s "$OUTPUT_FILE" ]; then
    echo "Output file info:"
    ls -lh "$OUTPUT_FILE"
    echo "You can play the recorded file with: ffplay $OUTPUT_FILE"
    
    # Check if there's a watch script and mention it
    if [ -f "${OUTPUT_FILE}_watch.sh" ]; then
      echo "Or use the generated script: sh ${OUTPUT_FILE}_watch.sh"
    fi
  else
    echo "No output file was created or it's empty."
    
    # Only show this message if file output was enabled
    if [ "$SAVE_TO_FILE" = "true" ]; then
      echo "Check the logs for errors."
      
      # Check if there are any other output files that might have been created
      echo "Checking for alternative output files..."
      find . -name "${OUTPUT_FILE}*" -type f -not -name "*.sh" | while read file; do
        if [ -s "$file" ]; then
          echo "Found potential output file: $file ($(du -h "$file" | cut -f1))"
          echo "You can try playing it with: ffplay $file"
        fi
      done
    fi
  fi
  
  exit
}

# Register cleanup function for SIGINT, SIGTERM
trap cleanup SIGINT SIGTERM EXIT

echo "Starting MOQ-Iroh demo..."
echo "-------------------------"

# Set output file - can be customized
OUTPUT_FILE="output.mp4"

# Option to save to file
SAVE_TO_FILE="true"

# Allow command-line options
while [[ $# -gt 0 ]]; do
  case $1 in
    --no-file)
      SAVE_TO_FILE="false"
      shift
      ;;
    --output|-o)
      OUTPUT_FILE="$2"
      SAVE_TO_FILE="true"
      shift 2
      ;;
    *)
      echo "Unknown option: $1"
      exit 1
      ;;
  esac
done

# Check for ffmpeg/ffplay
if ! command -v ffplay &> /dev/null; then
  echo "Warning: ffplay not found. Video playback may not work properly."
  echo "Please install ffmpeg: https://ffmpeg.org/download.html"
fi

# Check for required dependencies
if ! command -v cargo &> /dev/null; then
  echo "Error: cargo not found. Please install Rust: https://rustup.rs/"
  exit 1
fi

# Check if the examples exist
if [ ! -f "examples/publisher.rs" ] || [ ! -f "examples/subscriber.rs" ]; then
  echo "Error: publisher.rs or subscriber.rs not found in examples directory."
  echo "Make sure you're running this script from the root of the iroh-moq project."
  exit 1
fi

# Build the examples first to avoid compilation errors during the demo
echo "Building examples..."
cargo build --example publisher --example subscriber

if [ $? -ne 0 ]; then
  echo "Error: Failed to build examples. Please fix the compilation errors and try again."
  exit 1
fi

# Create log directories if they don't exist
mkdir -p logs

# Remove existing output file if it exists and we're saving to file
if [ "$SAVE_TO_FILE" = "true" ] && [ -f "$OUTPUT_FILE" ]; then
  echo "Removing existing output file: $OUTPUT_FILE"
  rm -f "$OUTPUT_FILE"
fi

# Start the publisher in the background and capture its output
echo "Starting publisher..."
RUST_LOG=debug cargo run --example publisher > logs/publisher_output.log 2>&1 &
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
    echo "Publisher failed to start. Check logs/publisher_output.log for details."
    cat logs/publisher_output.log | grep -E "error|failed|panic"
    exit 1
  fi
  
  # Extract node ID from publisher output
  NODE_ID=$(grep -o "Publisher node ID: [a-f0-9]\{64\}" logs/publisher_output.log | head -1 | cut -d' ' -f4)
  
  if [ -z "$NODE_ID" ]; then
    sleep 0.5
  fi
done

if [ -z "$NODE_ID" ]; then
  echo "Failed to get node ID from publisher. Check logs/publisher_output.log for details."
  cat logs/publisher_output.log | grep -E "error|failed|panic"
  exit 1
fi

echo "Publisher node ID: $NODE_ID"
echo "-------------------------"

# Get the current working directory for the subscriber script
CURRENT_DIR=$(pwd)

# Prepare the subscriber command based on if we're saving to file
SUBSCRIBER_CMD="RUST_LOG=debug cargo run --example subscriber -- $NODE_ID --play"
if [ "$SAVE_TO_FILE" = "true" ]; then
  SUBSCRIBER_CMD="$SUBSCRIBER_CMD -o $OUTPUT_FILE"
  echo "Saving video to: $OUTPUT_FILE"
else
  echo "Not saving video to file (streaming only)"
fi

# Create a script for the subscriber to run in a new terminal
cat > logs/run_subscriber.sh << EOF
#!/bin/bash
cd "$CURRENT_DIR"
echo "Starting subscriber with node ID: $NODE_ID"
if [ "$SAVE_TO_FILE" = "true" ]; then
  echo "Output file: $OUTPUT_FILE"
fi
echo "Press Ctrl+C to stop"
echo "-------------------------"
$SUBSCRIBER_CMD 2>&1 | tee logs/subscriber_output.log
echo "Subscriber exited"
EOF

# Make the script executable
chmod +x logs/run_subscriber.sh

# Create a manual command file that can be easily copied
cat > logs/subscriber_command.txt << EOF
# Run this command in a new terminal window:
cd "$CURRENT_DIR" && $SUBSCRIBER_CMD
EOF

# Print the manual command for reference
echo "If a new window doesn't open automatically, you can run this command in a new terminal:"
echo "-------------------------"
echo "cd \"$CURRENT_DIR\" && $SUBSCRIBER_CMD"
echo "-------------------------"
echo "This command is also saved in logs/subscriber_command.txt"

# Start the subscriber in a new terminal window based on OS
echo "Starting subscriber in a new terminal window..."

if [[ "$OSTYPE" == "darwin"* ]]; then
  # macOS - Create a separate AppleScript file and execute it
  cat > logs/open_terminal.scpt << EOF
tell application "Terminal"
  do script "cd '$CURRENT_DIR' && '$CURRENT_DIR/logs/run_subscriber.sh'"
end tell
EOF
  osascript logs/open_terminal.scpt
  echo "Subscriber started in a new Terminal window"
  
  # Save a placeholder PID for cleanup
  echo "0" > logs/subscriber_pid.txt
elif [[ "$OSTYPE" == "linux-gnu"* ]]; then
  # Linux - try different terminal emulators
  if command -v gnome-terminal &> /dev/null; then
    gnome-terminal -- bash -c "$CURRENT_DIR/logs/run_subscriber.sh; exec bash"
    echo "Subscriber started in a new GNOME Terminal window"
  elif command -v xterm &> /dev/null; then
    xterm -e "$CURRENT_DIR/logs/run_subscriber.sh; exec bash" &
    echo "Subscriber started in a new xterm window"
  elif command -v konsole &> /dev/null; then
    konsole -e "$CURRENT_DIR/logs/run_subscriber.sh; exec bash" &
    echo "Subscriber started in a new Konsole window"
  else
    echo "No supported terminal emulator found. Running subscriber in background."
    $CURRENT_DIR/logs/run_subscriber.sh &
    SUBSCRIBER_PID=$!
    echo $SUBSCRIBER_PID > logs/subscriber_pid.txt
  fi
else
  # Fallback for other systems
  echo "Unsupported OS for opening new terminal. Running subscriber in background."
  $CURRENT_DIR/logs/run_subscriber.sh &
  SUBSCRIBER_PID=$!
  echo $SUBSCRIBER_PID > logs/subscriber_pid.txt
fi

echo "-------------------------"
echo "Publisher running in this window, subscriber in new window."
echo "Press Ctrl+C in this terminal to stop both processes."
echo "-------------------------"

# Display publisher logs in real-time
echo "Publisher logs (from logs/publisher_output.log):"
echo "------------------------------------------------"
tail -f logs/publisher_output.log | grep --color=always -E 'Publisher node ID:.*|error|failed|capture|$'

# The cleanup function will handle termination 