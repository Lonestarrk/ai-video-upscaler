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
  - `audio/` the 5.1 / enhancement audio job (48 kHz f32, channel order FL FR FC LFE SL SR,
    `5.1(side)`): `source.rs` channels, codec, AC-3 Dolby Surround flag and dialnorm;
    `io.rs` exact-range extraction, WAV, streaming FFmpeg encoder; `sections.rs` 300 s
    sections with 2 s Hann crossfades; `classic.rs` STFT centre extraction + delayed,
    decorrelated surrounds; `ai.rs` + `pack.rs` the local AI pack (embeddable Python 3.12,
    ROCm or CPU PyTorch, MSST with BandIt Plus separation and Apollo repair; worker
    `assets/audio_ai.py`) in `tools\audio-ai`; `place.rs` stems → 5.1 (speech in FC at
    +3 dB, the ITU downmix FL + 0.7071·FC rebuilds the source; classic does the same);
    `lfe.rs` gated 20–90 Hz zero-phase LFE, 16/13/10 dB below the heard level, only bass
    rising above the scene's steady background (a drone gets none), octave-down synthesis
    for Biokänsla; `enhance.rs` band-splice of Apollo above the codec cutoff; `dsp.rs` filters; `job.rs` plan, resumable sections, FLAC + AC-3 encode, loudness
    (±1 LU), true peak (≤ -1 dBTP) and sync checks, cache in `<work>\audio\<key>`.
    `job.rs` (export) encodes the video to `video.mkv` in the job folder and
    `ffmpeg::mux` writes the MKV: FLAC 5.1 (default), AC-3 5.1, Original.
  - `hdr/` HDR export, pure Rust on the CPU (no GPU, no Vulkan): `itm.rs` the inverse tone
    mapping SDR → PQ/HLG BT.2020 (curve approved by the owner); `convert.rs` the stage
    `out/` (or `interp/`) → `hdr/%06d.png` 16-bit, parallel, cached by mode/peak/version in
    `hdr/settings.txt`, stale frames redone; `measure.rs` PQ math, per-frame MaxSCL /
    average maxRGB / percentiles, MaxCLL/MaxFALL; `hdr10plus.rs` the ST 2094-40 JSON for
    x265 `dhdr10-info` (scene ids from the export's cut list); `preview.rs` HDR → 100-nit
    sRGB for the frame browser; `mod.rs` `HdrEncode` and `x265_params`. `ffmpeg::encode`
    takes `EncodeOptions::hdr` (always libx265 10-bit). `examples/hdr_test.rs` runs a folder
    of SDR frames through this path.
- `crates/upscaler-gui` (egui 0.37 / eframe)
  - `app.rs` state, events, background workers' results; `worker.rs` threads + `Bus`.
  - `ui/viewer.rs` comparison (side by side / swipe, shared zoom-pan in the original's
    coordinates via `Layout`), `ui/timeline.rs`, `ui/side_panel.rs`, `ui/settings_window.rs`;
    `ui/audio.rs` the Ljud section, the Ljud-AI settings tab (install / remove / test) and
    audio export progress.
  - `i18n.rs` `tr("sv", "en")`; every visible text goes through it.

Data: settings `%APPDATA%\AI Video Upscaler\settings.json`; log, previews, export jobs and
downloaded FFmpeg in `%LOCALAPPDATA%\AI Video Upscaler\` (`upscaler.log`, `work\previews`,
`work\jobs\<key>\{src,out}`, `work\audio\<key>` audio jobs and `work\audio\stems` the stem
cache, `work\tmp\<id>` per request, `tools\bin`).

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

5.1 audio, verified 2026-11-01 on the owner's PC (RX 7900 XTX, ROCm pack) with the Voyager
pilot (AVI, AC-3 2.0 Dolby Surround, dialnorm -27, 23.976 fps) through
`examples/export_audio_test.rs` (Basic image provider, never the saved settings): teaser
0:00–2:00, bass event 1:00:25–1:00:45, bridge dialogue 16:40–18:20, theme 3:11–4:55 with AI and
Classic (Normal), bass with Biokänsla, bridge with enhancement only. Report and files:
`.superpowers/sdd/2026-11-01-surround-audio/task-14-report.md`.
- Time on the GPU: separation ~24 s per audio minute (47 s for 120 s), whole audio job + mux
  55 s for the 2-min teaser; Classic 4–13 s per range; Apollo stereo enhancement 36 s for 100 s.
- No job warnings. Loudness within 0.8 LU of the source (Classic exact), true peak ≤ -7.7 dBTP.
  Sync: 0 samples against the app's extraction, 0–1 against a decode-from-start reference, for
  FLAC and AC-3. FLAC has exactly the video's samples; AC-3 is 0.1–0.6 frame longer.
- MKV: FLAC 5.1 (default), AC-3 5.1, Original (copied, offset by its start time, 0.5 ms off).
- Levels, measured after the Classic STFT centre extraction, FC at +3 dB (ITU downmix
  `FL + 0.7071 FC = L`) and the background-relative LFE gate (AUDIO_VERSION 6; the Task 14
  numbers before these changes are superseded): bridge scene speech band FC +6.9 dB over FL
  with AI, +10.7 dB with Classic; bridge low/mid power sum original -10.22, AI -10.47,
  Classic -11.17; bass event LFE peak -33.0 / RMS -51.9 (Normal), -29.5 / -48.8 (Biokänsla);
  theme FC silent with AI (no speech); mains keep all their bass. Classic moves anything
  centred (also centred music or effects) to FC.
- Enhancement on this AC-3 source changes only >14 kHz, where the content is ~55 dB under the
  total: -3 dB at 14–16 kHz, +1.5–4 dB at 16–20 kHz. Practically inaudible here.
- Owner's listening so far (AI files): dialogue sits in the centre, LFE Normal is the right
  default, stereo enhancement "a little better". Classic after the centre rewrite not yet
  judged on the receiver.
- Separated stems are cached per range (`work\audio\stems\<key>\NNN\`, key without LFE level
  or engine), so an LFE A/B re-places the stems instead of separating again.

HDR export, verified 2026-11-02 on the owner's PC (i7-12700KF) through
`examples/hdr_test.rs` (the export's convert → measure → HDR10+ JSON → `ffmpeg::encode` with
`HdrEncode` → `ffmpeg::mux` with the Original track; default settings, HDR10+ at 1000 nits),
on SeedVR2 7B frames (1440×1080, 4:3, spike setup) of the Voyager pilot: lounge 838 s and
planet/eclipse 271 s, 240 frames each. Files in the owner's `tester\HDR\`.
- ffprobe: hevc Main 10, yuv420p10le, tv range, bt2020nc / smpte2084 / bt2020; mastering
  display BT.2020 primaries, L 1000 / 0.0001 nits; MaxCLL/MaxFALL lounge 1000/25, planet
  1000/345 (the eclipse flash). 240 frames at the source's 23.976 fps (10.010 s, as the
  original reference); container 10.029/10.035 s because the copied AC-3 ends ≤ 25 ms later.
- HDR10+: SMPTE2094-40 SEI on 240/240 frames, and every frame's maxscl / average_maxrgb /
  9 percentiles equal the JSON's frame in display order (ffprobe prints `JSON/100000`, i.e.
  the JSON's 0.1 cd/m² integer over 100000; nits = JSON / 10). Frame count = SceneInfo length.
  `-read_intervals %+#N` counts packets in decode order, so its frames 1.. are not JSON 1..
- Time per frame at 1440×1080: convert 16 ms, measure 25 ms, encode (x265 medium, CRF 16)
  27–30 ms. Disk: 16-bit `hdr/` frames 6.5–6.8 MB vs 1.54 MB SDR (~440 GB for a 45-min
  episode at this size, all on disk before encoding).
- Audio: `ffmpeg::encode`'s own audio input (`-ss` before `-i`, `-c:a copy`) keeps the
  pre-roll back to the seek point. With the AVI source the planet range got 5.9 s of audio
  before the video (video pts started at 5.9 s), the lounge 0.15 s. The example therefore
  muxes with `ffmpeg::mux` (two-stage seek, audio 13–19 ms late at most, AC-3 packet
  granularity). The plain SDR/HDR export with unchanged audio still uses the broken path.
- Not yet judged on the owner's projector. hdr10plus_tool not installed (not used).

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
