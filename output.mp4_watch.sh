#!/bin/sh
ffplay -loglevel warning -fflags nobuffer -flags low_delay -i /tmp/moq_stream_9316.h264 -vf "setpts=N/(30*TB)" -x 1280 -y 720 -window_title "MOQ-Iroh Stream"
