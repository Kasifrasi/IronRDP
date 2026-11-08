# IronRDP SDL viewer

`ironrdp-viewer-sdl` displays an RDP desktop through SDL3, with one resizable window per remote monitor.
It uses `ironrdp-client` for the connection and video decoding.
Mouse movement, buttons, scrolling, and physical keyboard input are supported.
Audio, clipboard, device redirection, and RemoteApp are outside its scope.

Install the SDL3 development library (3.2 or newer), a C compiler, and pkg-config before building.
On Linux, enable `openh264` to decode H.264 graphics; building the bundled decoder also requires NASM.
Windows uses the client's Media Foundation decoder by default.
With a compatible decoder, the viewer requests AVC444 preference for fine color detail; the server still selects the codec and compression quality.

```sh
cargo run -p ironrdp-viewer-sdl --features openh264 -- \
  server.example -u user --monitors 3 --fullscreen
```

The password is read from `RDP_PASSWORD` or prompted for interactively.
Certificates are validated by default; `--insecure` permits an untrusted server certificate.
`--primary-monitor` selects a zero-based monitor index, defaulting to the first window.
`--fullscreen` assigns one window to each local display, ordered from left to right, and negotiates its actual pixel resolution before connecting.
Without it, `--width` and `--height` set the initial window size.
The remote monitors form a horizontal row; subsequent window resizing requests matching remote resolutions through Display Control.
On high-DPI displays, remote pixels follow the window's pixel size while mouse positions use SDL's logical coordinates.
Ctrl+Alt+Enter toggles fullscreen for the focused window.
The keyboard layout defaults to German; use `--keyboard-layout 1033` for US English.
Switching away releases held keys and mouse buttons; input stays ordered when the client's queue is busy.
Closing any monitor window ends the connection.
`--no-video-optimization` keeps video on the graphics pipeline instead of accepting the separate video remoting channels; H.264 graphics remain available.
For a bounded local graphics recording and playback in FreeRDP, see [Graphics recording and visible replay].

Rendering follows [FreeRDP's SDL3 client]: persistent streaming textures, dirty-region uploads, persistent render targets, and disabled alpha blending for desktop pixels.
Each renderer stores only its monitor's image on the GPU.
SDL chooses the platform renderer; `SDL_RENDER_DRIVER` can override it, including `software` for diagnostics.
SDL video and texture operations run on the main thread, with bounded, ordered updates from the client runtime.

Run the rendering checks without a display server:

```sh
SDL_VIDEO_DRIVER=dummy SDL_RENDER_DRIVER=software \
  cargo test -p ironrdp-viewer-sdl --test video
```

[FreeRDP's SDL3 client]: https://github.com/FreeRDP/FreeRDP/blob/master/client/SDL/SDL3/sdl_window.cpp
[Graphics recording and visible replay]: ../ironrdp-client/README.md#graphics-recording-and-visible-replay
