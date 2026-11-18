# Video playback test

Run the same browser video and pause pattern in each RDP session to compare playback over TCP and UDP.
You need Python 3 and an FFmpeg build with libx264.

```sh
python3 testing/video-playback/serve.py
```

Open `http://127.0.0.1:18194/` in the browser running in the remote session if the server runs on the same machine.
If the server runs outside the VM, pass `--bind` with the host address reachable from the VM and use that address in the URL.

The server generates one video at startup and serves the same file for each run.
The page plays and pauses it 75 times, seeking every fifth cycle.
Use `?wait=3000` to give the browser three seconds before starting.
The page and server log show play and pause markers with the browser's cumulative video frame count.
Compare visible stalls alongside these markers, which measure playback inside the remote browser.
