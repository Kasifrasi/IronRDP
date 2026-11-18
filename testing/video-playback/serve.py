#!/usr/bin/env python3

import argparse
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import shutil
import subprocess
from tempfile import TemporaryDirectory


class PlaybackHandler(SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/mark/"):
            self.send_response(204)
            self.end_headers()
        else:
            super().do_GET()


def main():
    parser = argparse.ArgumentParser(description="Serve a repeatable video playback test")
    parser.add_argument("--bind", default="127.0.0.1", help="IPv4 address to listen on")
    parser.add_argument("--port", type=int, default=18194)
    args = parser.parse_args()

    if shutil.which("ffmpeg") is None:
        parser.error("ffmpeg with libx264 is required")

    with TemporaryDirectory(prefix="ironrdp-video-playback-") as directory:
        root = Path(directory)
        shutil.copyfile(Path(__file__).with_name("index.html"), root / "index.html")
        subprocess.run(
            [
                "ffmpeg", "-hide_banner", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc2=size=1600x900:rate=48",
                "-t", "15", "-c:v", "libx264", "-pix_fmt", "yuv420p",
                "-movflags", "+faststart", str(root / "probe.mp4"),
            ],
            check=True,
        )
        handler = partial(PlaybackHandler, directory=directory)
        with ThreadingHTTPServer((args.bind, args.port), handler) as server:
            print(f"Serving http://{args.bind}:{server.server_port}/", flush=True)
            try:
                server.serve_forever()
            except KeyboardInterrupt:
                pass


if __name__ == "__main__":
    main()
