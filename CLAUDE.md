# AI Video Upscaler

Desktop app (Windows first, macOS too) that restores and upscales video frame by frame with
AI image services, primarily ChatGPT through the user's ChatGPT subscription. Rust + egui,
same style as the owner's other project `Lonestarrk/stale-uninstaller`.

The owner speaks Swedish: answer in Swedish. Code, comments, commit messages and the README's
code parts are in English; every user-visible string exists in Swedish and English.

## Commands

```powershell
cargo test --workspace                     # unit + end-to-end tests (need ffmpeg on PATH, else skipped)
cargo build -p upscaler-gui --release      # target\release\AIVideoUpscaler.exe
cargo run -p upscaler-gui -- path\to\video # opens a video at start
cargo fmt --all                            # run before committing
```

- Windows needs the MSVC toolchain (VS "Desktop development with C++"); `.cargo/config.toml`
  links the CRT statically. Renderer: wgpu on Windows/macOS, glow (OpenGL) elsewhere or with
  `UPSCALER_RENDERER=glow` (old GPUs, VMs, RDP).
- Test videos: `powershell -ExecutionPolicy Bypass -File scripts\make-test-videos.ps1`
  (Sintel trailer + DVD variants into `test-videos\`, git-ignored). Frame accuracy, black bars
  and the aspect check on real files (`;` between files on Windows, `:` elsewhere):
  ```powershell
  $env:UPSCALER_TEST_VIDEOS = (Get-ChildItem test-videos\* | % FullName) -join ";"
  cargo test -p upscaler-core --test export frame_accuracy -- --ignored --nocapture
  ```
- CI: `.github/workflows/build.yml` builds the exe and the Inno Setup installer (with FFmpeg)
  on every push; artifacts under Actions. Installer script: `installer/upscaler.iss`.

## Layout

- `crates/upscaler-core` (no UI)
  - `ffmpeg.rs` probe (`VideoInfo`: display size in square pixels, stored size, SAR,
    interlacing, crop, rotation), frame grab, range extraction, encode with the source audio,
    black-bar detection (`cropdetect` at 10 points, union of samples).
  - `aspect.rs` "is the file flagged with the wrong aspect ratio?" check + suggestions.
  - `frame.rs` padding to model aspect ratios and cropping back, canvas extension for
    "change aspect ratio", fingerprints for duplicate / scene-cut detection.
  - `prompt.rs` technical rules appended to the user's prompt (framing, padding, reference
    frame, extension).
  - `provider/` one file per service, `Provider` trait (`upscale`, `canvas_aspects`):
    `codex.rs` (ChatGPT subscription via Codex CLI), `openai_api.rs`, `gemini.rs`, `flux.rs`,
    `grok.rs`, `realesrgan.rs`, `basic.rs`; `http.rs` shared HTTP helpers and a mock server
    for tests. `upscale_frame` in `provider/mod.rs` wraps every service: extend → pad →
    prompt → call → crop → resize to the work size.
  - `job.rs` geometry (`work` square-pixel size, `stored` size, SAR for anamorphic output),
    preview cache, export job (extract → analyse → upscale → encode), resumable by job folder.
  - `settings.rs` all settings (JSON in `%APPDATA%\AI Video Upscaler`), prompts, migration.
- `crates/upscaler-gui` (egui 0.37 / eframe)
  - `app.rs` state, events, background workers' results; `worker.rs` threads + `Bus`.
  - `ui/viewer.rs` comparison (side by side / swipe, shared zoom-pan in the original's
    coordinates via `Layout`), `ui/timeline.rs`, `ui/side_panel.rs`, `ui/settings_window.rs`.
  - `i18n.rs` `tr("sv", "en")`; every visible text goes through it.

Data: settings `%APPDATA%\AI Video Upscaler\settings.json`; log, previews, export jobs and
downloaded FFmpeg in `%LOCALAPPDATA%\AI Video Upscaler\` (`upscaler.log`, `work\previews`,
`work\jobs\<key>\{src,out}`, `work\tmp\<id>` per request, `tools\bin`).

## How the pieces fit

- Everything downstream works in square pixels at the display size. Anamorphic sources (DVD)
  are stretched on decode and squeezed back on encode (`keep_anamorphic`). Interlaced sources
  are deinterlaced (bwdif). Frames are picked by timestamp after a 1 s pre-roll with `-copyts`
  (plain accurate seek was off by one on MPEG-2); preview frames are pixel-identical to a
  sequential decode (tested).
- Export: frames in order with the previous *upscaled* frame as reference (when the service
  takes two images and "use previous frame" is on), none after a scene cut; near-identical
  frames reuse the previous result; optional every 2nd/3rd frame + `minterpolate`.
- Caches are keyed by the video identity (path, size, mtime, crop, SAR override) and every
  setting that changes pixels, so changing a setting never shows a stale result.

## ChatGPT subscription path (the main feature)

`codex exec --skip-git-repo-check --color never --sandbox <mode> --cd <tmp> --image=input.png
[--image=reference.png] [-m model] -c model_reasoning_effort=low -` with the instruction on
stdin. The agent is told to use its built-in image tool once and save `output.png` in the
folder; if it does not, the newest image under `$CODEX_HOME/generated_images` created during
the turn is taken. `OPENAI_API_KEY`/`CODEX_API_KEY` are removed from Codex's environment so the
plan is used, not API billing. A failed preview keeps its tmp folder (`codex-output.txt` has
Codex's transcript); the error message says where.

## Status

Verified here (Linux container, real ffmpeg, Xvfb screenshots): loading, timeline, frame
stepping, previews with the basic provider, both comparison modes, export with audio and
resume, DVD handling (SAR, interlacing, letterbox, wrong-flag detection), aspect-ratio
extension layout, all HTTP services against a mock server, and a stand-in `codex` script.

NOT verified against the real services (no accounts in the dev container). This is what to
test first on the owner's Windows PC:

1. ChatGPT via Codex: install (`npm i -g @openai/codex` or the app's button), "Sign in with
   ChatGPT", preview one frame. Check: does `codex exec` accept the flags above on Windows
   (`codex.cmd` through npm)? Does the agent call the image tool and write `output.png`, or
   does the fallback to `generated_images` kick in? Is the Windows sandbox OK
   (`workspace-write`), or is `danger-full-access` needed? Does the reference image actually
   reach the image tool (watch for flicker between frames in a short export)?
   Time per frame and plan limits.
2. Gemini: `generateContent` with `gemini-3-pro-image` (Google's docs now lead with an
   Interactions API; if generateContent is refused, switch `provider/gemini.rs`).
3. FLUX: base64 `input_image` is documented for Kontext; for FLUX.2 it is assumed.
4. Grok: `response_format: b64_json` is assumed; a URL answer is handled too.
5. OpenAI API: `gpt-image-2` edits with two `image[]` parts; `input_fidelity` is optional.
6. The release build with MSVC, the installer from CI, and the app on a high-DPI screen.

Ideas discussed and not built: AI check of "does this frame look stretched?"; a preview that
uses the previous preview as reference (to judge consistency before exporting); Topaz Labs API
as a faithful non-generative upscaler; trying the Gemini Interactions API.

## Conventions

- Tests: core logic has unit tests; anything touching ffmpeg goes in
  `crates/upscaler-core/tests/export.rs` (skips when ffmpeg is missing). New services get a
  mock-server test like `provider/gemini.rs`.
- Commits: what changed and why, in English. Do not put model names of the assistant in commits.
- The owner wants a working app over discussion: build, test, then show (screenshots or a
  short summary) and ask.
