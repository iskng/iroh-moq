#!/bin/bash
cd "/Users/user/dev/test/iroh-moq"
echo "Starting subscriber with node ID: 4973308fa089d04836044b95759b461381d08fac94230941d8fd0db48641fcf0"
echo "Output file: output.mp4"
echo "Press Ctrl+C to stop"
echo "-------------------------"
RUST_LOG=debug cargo run --example subscriber -- 4973308fa089d04836044b95759b461381d08fac94230941d8fd0db48641fcf0 -o output.mp4 --auto-play 2>&1 | tee logs/subscriber_output.log
echo "Subscriber exited"
