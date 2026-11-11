#!/usr/bin/env python3
"""Time to ready: wait for START_FILE (nanoseconds since the epoch, written just before the server is launched), then poll
connect() on 127.0.0.1:PORT every 0.5 ms and print the milliseconds from that start to the first accepted connection.
Usage: d-ready.py START_FILE PORT"""
import os, socket, sys, time
f, port = sys.argv[1], int(sys.argv[2])
while not os.path.exists(f): time.sleep(0.0005)
start = int(open(f).read())
end = time.time() + 120
while time.time() < end:
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=0.2); s.close()
        print((time.time_ns() - start) / 1e6); sys.exit(0)
    except OSError: time.sleep(0.0005)
print(-1); sys.exit(1)
