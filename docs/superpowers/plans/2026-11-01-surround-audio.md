# Surround Audio (5.1 from stereo + better audio quality) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an export turn the source's stereo into real 5.1 (local AI separation by default, classic DSP as an alternative), optionally improve audio quality, and write an MKV with FLAC 5.1 + AC-3 5.1 + the untouched original.

**Architecture:** A new `audio` module in `upscaler-core` runs an audio job per project in sections (resumable, cached under `<project>/audio/<key>/`), with DSP in Rust (filters, LFE, placement, stitching, band-splice) and AI work delegated to a locally downloaded Python pack (MSST + BandIt Plus + Apollo) behind an `AudioAi` trait. The export encodes video without audio to an intermediate file and muxes it with the audio job's tracks; changing only audio settings re-runs audio + mux, not the frames.

**Tech Stack:** Rust 2021, egui 0.37, FFmpeg CLI, `hound` (WAV), embeddable CPython 3.11 + PyTorch (AMD ROCm for Windows, else CPU) + ZFTurbo Music-Source-Separation-Training.

**Spec:** `docs/superpowers/specs/2026-11-01-surround-audio-design.md` (Swedish). Background numbers: `C:\Users\Jimmy\AppData\Local\Temp\claude\H--Projects-ai-video-upscaler\21068c7e-69e6-4910-8845-4d0398a6ed17\scratchpad\audio-research\report.md`.

## Global Constraints

- Read `CLAUDE.md` first. Code, comments, commits in English; every user-visible string through `tr("sv", "en")`; owner reads Swedish.
- Commit messages: what changed and why; end with the attribution lines the session gives. Never name the assistant model in the message body.
- Run `cargo fmt --all` and `cargo test --workspace` before every commit; tests touching ffmpeg go in `crates/upscaler-core/tests/` and skip when ffmpeg is missing (follow `tests/export.rs`).
- Sample rate inside the audio job: **48 000 Hz**, 32-bit float. Channel order everywhere: **FL FR FC LFE SL SR**, layout `5.1(side)`.
- **Sync:** the audio job covers exactly the exported frames: start = `info.frame_time(start)`, length = `(end - start) * fps_den / fps_num` seconds, converted to samples with `round(seconds * 48000)`, padded or trimmed to that exact count. The exported video keeps the source frame rate exactly (`fps_num/fps_den`).
- Never put the same signal in FC and FL/FR with different delays. Every parallel path is latency-compensated.
- LFE: low-pass **90 Hz**, high-pass **20 Hz**, zero-phase (forward-backward IIR), never above 120 Hz; written 10 dB below the heard level; peaks ≤ −1 dBFS; mains keep all their bass.
- Never process LFE with enhancement. Never use PCM in the output container. UI says "AC-3", never "Dolby".
- Defaults: `audio_mode = Original`; when the user switches to 5.1, `surround_engine = LocalAi`; `lfe_level = Normal`; `audio_enhance = false`. Local AI runs on the GPU when available, else on the CPU with a time warning.
- AI pack location: `%LOCALAPPDATA%\AI Video Upscaler\tools\audio-ai` (`paths::tools_dir().join("audio-ai")`).
- The app is private: all models may be used regardless of licence; download them from their original sources.

## Review Focus

1. **Short ranges and odd frame rates** (I/O range of a few seconds, 30000/1001 source): audio and video must have the same length and the output keeps the source frame rate → tests in Task 12.
2. **Resuming after cancel mid-section** (a half-written section file): must not be used; job continues at the first incomplete section → test in Task 11.
3. **Mono, already-5.1 and non-AC-3 sources:** 5.1-from-stereo is unavailable for mono, source LFE untouched for 5.1, AAC/MP3 stereo works → tests in Tasks 2 and 11.
4. **Dialogue opening the subwoofer** (a deep male voice with energy at 100–300 Hz on the classic path) → test in Task 4.
5. **AI pack missing or broken while LocalAi is selected:** export must stop before any frame work with a clear message, not after hours → test in Task 12.

---

### Task 0: Baseline branch

**Files:** none new.

- [ ] **Step 1:** `git status`; create branch `surround-audio` from the current `main`; commit the pending work already in the tree (aspect-ratio fix, DX12, prompt rules, 2.35:1, projects, quality control, frame browser, test tools, `.gitignore`, the spec and this plan) as separate logical commits.
- [ ] **Step 2:** `cargo test --workspace` → all pass.

### Task 1: Audio settings in Settings and Project

**Files:**
- Modify: `crates/upscaler-core/src/settings.rs` (new enums + fields + defaults)
- Modify: `crates/upscaler-core/src/project.rs` (add the fields to `ProjectSettings` and the `project_fields!` list)
- Test: same files' `tests` modules

**Interfaces:**
- Produces:
  - `enum AudioMode { Original, Surround }` (serde, Copy, PartialEq), default `Original`.
  - `enum SurroundEngine { LocalAi, Classic }`, default `LocalAi`.
  - `enum LfeLevel { Subtle, Normal, Strong, Cinema }`, default `Normal`; `fn file_gain_db(self) -> f32` = −16, −13, −10, −10; `fn synth(self) -> bool` = true only for `Cinema`.
  - `Settings` fields: `audio_mode`, `surround_engine`, `lfe_level`, `audio_enhance: bool`.
  - `Settings::audio_active(&self) -> bool` = `audio_mode == Surround || audio_enhance`.

- [ ] **Step 1: Failing tests** `audio_defaults` (defaults as above, `audio_active()` false) and in project.rs `project_keeps_audio_choices` (set Surround/Classic/Cinema/enhance, save, load, apply → equal).
- [ ] **Step 2:** `cargo test -p upscaler-core --lib audio_defaults project_keeps_audio_choices` → FAIL (unknown type).
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Tests pass; `cargo test --workspace` green.
- [ ] **Step 5:** Commit.

### Task 2: Source audio analysis

**Files:**
- Create: `crates/upscaler-core/src/audio/mod.rs` (module root, `pub mod` lines), `crates/upscaler-core/src/audio/source.rs`
- Modify: `crates/upscaler-core/src/lib.rs` (`pub mod audio;`)
- Test: `source.rs` unit tests + `crates/upscaler-core/tests/audio.rs` (ffmpeg)

**Interfaces:**
- Produces:
  - `struct Ac3Header { acmod: u8, dolby_surround: bool, dialnorm: i8 }`; `fn parse_ac3(frame: &[u8]) -> Option<Ac3Header>` (sync word 0x0B77; bsid/bsmod/acmod; when acmod == 2 the next 2 bits are dsurmod, `dolby_surround = dsurmod == 2`; dialnorm 5 bits → `-(value)` dB, 0 means −31).
  - `struct SourceAudio { channels: u32, codec: String, dolby_surround: bool, dialnorm: Option<i8> }`; `fn analyse(tools: &Tools, info: &VideoInfo) -> Result<Option<SourceAudio>>` (None when no audio; reads the first AC-3 frame via `ffmpeg -i src -map 0:a:0 -c copy -frames:a 1 -f ac3 pipe:1` when codec is ac3).
  - `SourceAudio::can_upmix(&self) -> bool` = `channels == 2`; `is_surround(&self) -> bool` = `channels >= 6`.

- [ ] **Step 1: Failing tests:** `parses_dolby_surround_header` (hand-built 2/0 header with dsurmod=2, dialnorm 27 → `dolby_surround` true, `dialnorm == -27`); `rejects_non_ac3` (random bytes → None). In `tests/audio.rs`: `detects_flag_and_layouts` — generate with ffmpeg 2 s stereo AC-3 `-dsur_mode on`, mono AAC, 5.1 AC-3; assert flags, `can_upmix` true/false/false, `is_surround` only for 5.1.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Pass. Also run once against the Voyager file manually: `dolby_surround == true`, `dialnorm == Some(-27)`.
- [ ] **Step 5:** Commit.

### Task 3: DSP primitives

**Files:**
- Create: `crates/upscaler-core/src/audio/dsp.rs`

**Interfaces:**
- Produces (all on `&[f32]` mono at a given `rate: f32`):
  - `struct Biquad` (RBJ cookbook, Direct Form II transposed) with `lowpass(f, q, rate)`, `highpass(f, q, rate)`; `fn lr4_lowpass(x, f, rate) -> Vec<f32>` / `lr4_highpass` (two cascaded Butterworth Q=0.7071); `fn zero_phase(x, filter: impl Fn(&[f32]) -> Vec<f32>) -> Vec<f32>` (forward, reverse, forward, reverse).
  - `fn band(x, lo, hi, rate) -> Vec<f32>` (zero-phase LR4 band-pass).
  - `fn envelope_db(x, rate, attack_ms, release_ms) -> Vec<f32>` (peak follower, dBFS).
  - `fn delay(x, samples: usize) -> Vec<f32>` (same length, zero-filled start).
  - `fn decorrelate(x, seed: u32) -> Vec<f32>` (cascade of 4 first-order all-passes with seed-dependent coefficients in 0.3..0.7).
  - `fn db_to_gain(db) -> f32`, `fn rms_db(x) -> f32`.

- [ ] **Step 1: Failing tests:** `lowpass_90_passes_40_blocks_300` (sine 40 Hz loses < 1 dB, 300 Hz loses > 30 dB through zero-phase LR4 90 Hz); `zero_phase_keeps_peak_position` (impulse at 24000 stays at 24000 ±1); `band_isolates_range`; `envelope_follows_burst` (0.5 s of −20 dBFS 50 Hz after silence → envelope within 2 dB of −20 by 20 ms after onset); `decorrelated_pair_is_different_but_same_energy` (correlation < 0.5, RMS within 0.5 dB).
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** Pass. **Step 5:** Commit.

### Task 4: LFE generation

**Files:**
- Create: `crates/upscaler-core/src/audio/lfe.rs`

**Interfaces:**
- Consumes: Task 3 DSP; `LfeLevel` (Task 1).
- Produces: `enum LfeSource { Effects, MonoSum }`; `fn make_lfe(input: &[f32], source: LfeSource, level: LfeLevel, rate: f32) -> Vec<f32>`.
  - Gate on `band(input, 30, 100)` envelope: open above **−45 dBFS**, attack **5 ms**, release **250 ms**, closed range **−20 dB**.
  - `MonoSum` (classic path) additionally requires `band(30,60)` energy ≥ `band(100,300)` energy − 6 dB to open (voice guard), and its threshold is **−40 dBFS**. `Cinema` with `MonoSum` behaves as `Strong` (no synthesis).
  - Signal: `zero_phase(LR4 HP 20)` then `zero_phase(LR4 LP 90)` of the gated input, times `db_to_gain(level.file_gain_db())`.
  - `Cinema` (Effects only): octave-down synth: detect `band(50, 90)`, flip a ±1 square on each rising zero crossing (halves the frequency), multiply by the band's envelope, LP 55 Hz, HP 20 Hz, mix at **−9 dB** relative to the detection band, only while the gate is open.
  - Final soft limit so peaks ≤ −1 dBFS.

- [ ] **Step 1: Failing tests:** `silent_in_dialogue` (mono sum of a 120 Hz + 220 Hz harmonic "voice" at −20 dBFS through `MonoSum`/`Normal` → LFE RMS < −60 dBFS); `explosion_reaches_lfe` (0.4 s 45 Hz burst at −18 dBFS → LFE RMS ≈ −18 −13 dB ±2); `levels_step_by_3db` (Subtle/Normal/Strong differ by 3 dB ±0.3); `cinema_adds_subharmonic` (70 Hz burst with `Effects`/`Cinema` → energy at 35 Hz ≥ 20 dB above the same at `Strong`); `nothing_above_120hz` (white noise → band 150–1000 Hz in LFE < −50 dB relative); `peaks_capped`.
- [ ] **Step 2–4:** Fail, implement, pass. **Step 5:** Commit.

### Task 5: Sections and stitching

**Files:**
- Create: `crates/upscaler-core/src/audio/sections.rs`

**Interfaces:**
- Produces:
  - `const SECTION_SECONDS: u64 = 300; const OVERLAP_SECONDS: u64 = 2;`
  - `struct Section { index: usize, start: u64, len: u64 }` (samples, including overlap at both inner ends); `fn plan(total: u64, rate: u32) -> Vec<Section>`.
  - `fn stitch(sections: &[Section], total: u64, channels: usize, read: impl FnMut(&Section) -> Result<Vec<f32>>, write: impl FnMut(&[f32]) -> Result<()>) -> Result<()>` — interleaved samples; equal-power crossfade across each overlap; writes exactly `total * channels` samples; holds at most two sections in memory.

- [ ] **Step 1: Failing tests:** `sections_cover_everything` (for totals 1, 300·48000, 92·60·48000+7: union covers 0..total, each overlap = 2·48000 except edges); `stitching_copies_reconstructs_source` (sections cut from a known 6-channel ramp signal, stitched → identical within 1e-5); `short_range_single_section` (3 s total → one section, no crossfade).
- [ ] **Step 2–4.** **Step 5:** Commit.

### Task 6: Audio I/O through FFmpeg and WAV

**Files:**
- Modify: `crates/upscaler-core/Cargo.toml` (add `hound = "3"`)
- Create: `crates/upscaler-core/src/audio/io.rs`
- Test: `crates/upscaler-core/tests/audio.rs`

**Interfaces:**
- Produces:
  - `struct Pcm { rate: u32, channels: usize, samples: Vec<f32> }` (interleaved) with `frames()`, `channel(i) -> Vec<f32>`, `from_channels(rate, &[Vec<f32>]) -> Pcm`.
  - `fn extract(tools: &Tools, src: &Path, start: f64, dur: f64, out_wav: &Path, cancel: &AtomicBool) -> Result<()>` — first audio stream, resampled to 48 kHz f32, original channel count, length exactly `round(dur*48000)` frames (pad/trim).
  - `fn read_wav(path) -> Result<Pcm>`, `fn write_wav(path, &Pcm) -> Result<()>` (write to `*.part` then rename — atomic, so half-written files are never seen).
  - `fn encode_stream(tools, channels: usize, layout: &str, codec: AudioCodec, out: &Path, cancel) -> Result<StreamEncoder>`; `StreamEncoder::write(&[f32])`, `finish()` — pipes raw f32le into ffmpeg (`-f f32le -ar 48000 -ac N -channel_layout <layout>`); `enum AudioCodec { Flac24, Ac3_640 { dialnorm: i8 } }`.

- [ ] **Step 1: Failing tests (ffmpeg):** `extract_is_sample_exact` (1.2345 s from a 3 s sine → exactly 59256 frames at 48 kHz); `wav_round_trip`; `flac_51_keeps_layout` (stream 6 distinct sines → FLAC; ffprobe says 6 channels, `5.1(side)`; each channel's dominant frequency is in the right place).
- [ ] **Step 2–4.** **Step 5:** Commit.

### Task 7: Classic 5.1

**Files:**
- Create: `crates/upscaler-core/src/audio/classic.rs`
- Test: `crates/upscaler-core/tests/audio.rs`

**Interfaces:**
- Consumes: Tasks 3–6.
- Produces: `fn upmix_classic(tools: &Tools, stereo_wav: &Path, level: LfeLevel, cancel) -> Result<Pcm>` (6 channels, same frame count as input), built on `fn upmix_classic_raw(tools, stereo_wav, cancel) -> Result<Pcm>` (the latency-compensated `surround` output before the SL/SR delay, decorrelation and LFE; `pub(crate)` for tests). FL/FR/FC/SL/SR from `ffmpeg surround=chl_out=5.1:lfe=0:win_size=4096:overlap=0.5:smooth=0.1`, BL/BR relabelled SL/SR, the 2048-sample filter latency removed (`atrim=start_sample=2048` after `apad`), SL/SR delayed 15 ms and decorrelated (seeds 1, 2); LFE = `make_lfe(mono sum, MonoSum, level)`.

- [ ] **Step 1: Failing tests:** `classic_downmix_nulls` (10 s stereo of speech-like tones plus noise: on `upmix_classic_raw`, `FL+0.7071·FC+0.7071·SL` minus L has RMS ≤ **−35 dBFS**; with the 2048-sample compensation removed it is at least 10 dB worse); `classic_is_in_sync` (cross-correlation of FL+FR+FC against L+R peaks at lag 0 ±1); `classic_length_matches`.
- [ ] **Step 2–4.** **Step 5:** Commit.

### Task 8: Local AI pack and runner

**Files:**
- Create: `crates/upscaler-core/src/audio/ai.rs` (trait + `LocalAi`), `crates/upscaler-core/src/audio/pack.rs` (download/install/status), `crates/upscaler-core/assets/audio_ai.py` (worker script, embedded with `include_str!` and written into the pack)
- Test: unit tests in `ai.rs`/`pack.rs`; real run is manual (Step 6)

**Interfaces:**
- Produces:
  - `struct Stems { speech: PathBuf, music: PathBuf, effects: PathBuf }`.
  - `trait AudioAi: Send + Sync { fn separate(&self, wav: &Path, out_dir: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(f32)) -> Result<Stems>; fn enhance(&self, wav: &Path, out: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(f32)) -> Result<()>; }`
  - `struct LocalAi { python: PathBuf, script: PathBuf, device: Device }`, `enum Device { Gpu(String), Cpu }`; `LocalAi::open() -> Result<LocalAi>` (error text in English, shown by the GUI, when the pack is missing).
  - `pack::status() -> PackStatus { installed: bool, device: Option<Device>, size_bytes: u64 }`, `pack::install(cancel, progress: impl FnMut(&str, f32)) -> Result<()>`, `pack::remove() -> Result<()>`.
  - `audio_ai.py` commands: `device` → prints `{"device":"gpu","name":"..."}` or `{"device":"cpu"}` (`torch.cuda.is_available()`; ROCm builds report AMD GPUs through this API); `separate IN OUT_DIR` → writes `speech.wav music.wav effects.wav` via MSST `model_type bandit`, config `config_dnr_bandit_bsrnn_multi_mus64.yaml`, checkpoint `model_bandit_plus_dnr_sdr_11.47.chpt`; `enhance IN OUT` → Apollo through MSST. Progress lines `PROGRESS 0.42` on stdout; errors on stderr with exit code ≠ 0.
- Install steps (record exact URLs as constants after verifying them in Step 1): python.org **embeddable CPython 3.11 amd64** zip → enable `site` in `python311._pth` → `get-pip.py` → `pip install` AMD's **PyTorch for Windows (ROCm)** wheels for gfx1100 (per AMD release notes RN-AMDGPU-WINDOWS-PYTORCH, newest available); if that install fails or `device` reports cpu, install CPU torch from `https://download.pytorch.org/whl/cpu` → MSST source zip from GitHub (`ZFTurbo/Music-Source-Separation-Training`) + its requirements → model checkpoints/configs from the URLs in MSST `docs/pretrained_models.md`.

- [ ] **Step 1: Spike (throwaway, not committed):** on this machine, find and verify the current AMD ROCm Windows torch wheel URLs for the RX 7900 XTX and that `torch.cuda.is_available()` is true; run MSST BandIt Plus on `scratchpad/audio-research/sample_teaser_0-120s.wav` and note speed on GPU and CPU. Write the findings (URLs, versions, speeds) into this plan under "Spike findings" before continuing.
- [ ] **Step 2: Failing unit tests:** `parses_progress_lines`, `parses_device_json` (gpu with name / cpu), `missing_pack_is_a_clear_error` (`LocalAi::open` with an empty dir → error mentions the settings button).
- [ ] **Step 3:** Implement pack + runner + script (pack install skips steps already done; downloads to `*.part` then rename).
- [ ] **Step 4:** Tests pass.
- [ ] **Step 5:** Commit.
- [ ] **Step 6 (manual):** install the pack through a small example `cargo run -p upscaler-core --release --example audio_pack -- install` then `-- device` → prints the GPU; `-- separate <teaser wav> <dir>` produces three stems that sound right.

### Task 9: 5.1 placement from stems

**Files:**
- Create: `crates/upscaler-core/src/audio/place.rs`

**Interfaces:**
- Consumes: Tasks 3, 4, 6 (`Pcm`).
- Produces: `fn place(src: &Pcm, speech: &Pcm, music: &Pcm, effects: &Pcm, level: LfeLevel) -> Pcm` (6 ch, src length):
  - residual = src − (speech + music + effects), per channel.
  - FC = HP 70 Hz of mono(speech) = (L+R)/2.
  - FL/FR = music + effects + residual + (speech − mono(speech)).
  - SL/SR = decorrelate(delay(15 ms, band(100, 7000, (ME_L − ME_R)/2)), seeds 1/2) at −3 dB, where ME = music + effects.
  - LFE = `make_lfe(mono(effects), Effects, level)`.

- [ ] **Step 1: Failing tests:** `speech_goes_to_centre_only` (speech-only stems: FC carries it, FL/FR/SL/SR < −40 dB relative, LFE silent); `nothing_lost` (downmix FL+0.7071·FC vs L for random stems: residual ≤ −30 dBFS excluding SL/SR contribution); `effects_bass_reaches_lfe`; `output_length_and_order`.
- [ ] **Step 2–4.** **Step 5:** Commit.

### Task 10: Quality enhancement (band-splice)

**Files:**
- Create: `crates/upscaler-core/src/audio/enhance.rs`

**Interfaces:**
- Produces:
  - `fn detect_cutoff(x: &Pcm) -> f32` — frequency where the smoothed spectrum (FFT 8192, averaged) first falls **15 dB** below the 8–12 kHz average; clamp 10–20 kHz; default 20 kHz when no drop.
  - `fn splice(original: &Pcm, model: &Pcm, cutoff: f32) -> Pcm` — `LP(cutoff)` of original + `HP(cutoff)` of model, both zero-phase LR4, model band level capped so its 1/3-octave levels do not exceed the original's 6–cutoff slope extended at **−12 dB/octave**.
  - `fn to_mid_side(&Pcm) -> Pcm`, `fn from_mid_side(&Pcm) -> Pcm` (used for Dolby Surround stereo).
  - `fn enhance_pcm(ai: &dyn AudioAi, pcm: &Pcm, dolby_surround: bool, tmp: &Path, cancel) -> Result<Pcm>` — writes WAV, `ai.enhance`, reads back, splices; M/S path when `dolby_surround`.

- [ ] **Step 1: Failing tests:** `finds_14k_cutoff` (noise low-passed at 14 kHz → 13.5–14.5 kHz); `below_cutoff_is_untouched` (splice with a model output that is pure noise: band 100 Hz–12 kHz equals original within 0.1 dB); `mid_side_round_trip`; `enhance_with_fake_ai` (fake `AudioAi` that adds 16 kHz tone → present after splice, capped).
- [ ] **Step 2–4.** **Step 5:** Commit.

### Task 11: The audio job

**Files:**
- Create: `crates/upscaler-core/src/audio/job.rs`
- Test: unit tests with a fake `AudioAi`; `tests/audio.rs` for an ffmpeg end-to-end on a classic job

**Interfaces:**
- Consumes: Tasks 1–10.
- Produces:
  - `struct AudioSpec { info: VideoInfo, start: u64, end: u64, settings: Settings }`; `fn key(spec) -> String` (video identity, start, end, `audio_mode`, `surround_engine`, `lfe_level`, `audio_enhance`, `AUDIO_VERSION: u32 = 1`); `fn job_dir(spec) -> PathBuf` = `settings.work_dir()/audio/<key>`.
  - `enum AudioTrack { Flac51(PathBuf), Ac3_51(PathBuf), FlacStereo(PathBuf) }`; `struct AudioResult { tracks: Vec<AudioTrack>, warnings: Vec<String> }`.
  - `fn run(tools, spec, ai: Option<&dyn AudioAi>, cancel, emit: &(dyn Fn(AudioEvent) + Sync)) -> Result<AudioResult>`; `enum AudioEvent { Stage(AudioStage), Section { done: usize, total: usize }, Warning(String) }`, `enum AudioStage { Extracting, Separating, Enhancing, Placing, Joining, Checking, Encoding }`.
  - Flow: extract range → for each section not yet in `sections/NNN.wav` (complete files only, atomic writes): separate (LocalAi) or classic, optional enhance, place → stitch into `FlacStereo`/`Flac51` via `encode_stream` → AC-3 from the FLAC (`Ac3_640 { dialnorm }`, source dialnorm or −27) when 5.1 → checks.
  - Rules: source `is_surround()` → no upmix; enhance processes FL/FR, FC, SL/SR (pairs), LFE untouched; source mono with `Surround` → error "mono source cannot be turned into 5.1" before any work.
  - Checks (warn, never fail): loudness of the result vs source within ±1 LU, else apply gain and re-encode; true peak ≤ −1 dBTP via limiter; sync: cross-correlation of the result's downmix vs source at lag 0 ±1 sample, else warning.

- [ ] **Step 1: Failing tests:** `resumes_after_partial_section` (fake AI counts calls; first run cancelled after 2 of 3 sections with a `.part` file left; second run calls AI only for section 3); `key_changes_with_lfe_level`; `five_one_source_keeps_lfe` (6-ch source + enhance: LFE samples identical to source's); `mono_surround_rejected_early`; e2e `classic_job_writes_three_tracks` (ffmpeg: 20 s stereo test input → FLAC 5.1 + AC-3 5.1 files with right layouts and exact length).
- [ ] **Step 2–4.** **Step 5:** Commit.

### Task 12: Export integration and MKV mux

**Files:**
- Modify: `crates/upscaler-core/src/job.rs` (export flow, `Stage::Audio`), `crates/upscaler-core/src/ffmpeg.rs` (`encode` with `audio_from: None` to an intermediate; new `mux`)
- Test: `crates/upscaler-core/tests/export.rs`

**Interfaces:**
- Consumes: Task 11.
- Produces:
  - `ffmpeg::mux(tools, video: &Path, tracks: &[AudioTrack], original: Option<(PathBuf, f64, f64)>, out: &Path, cancel) -> Result<()>` — MKV; video copied; track order per spec table (FLAC first, `-disposition:a:0 default`, then AC-3, then the original copied); titles "5.1 (FLAC)", "5.1 (AC-3)", "Original".
  - Export: when `!settings.audio_active()` → unchanged MP4 path. Otherwise video is encoded without audio to `<job>/video.mkv` (skipped when it exists and `<job>/video.done` matches the frame job), then `audio::job::run`, then `mux` to the output (extension forced to `.mkv`). Before any frame work, when `surround_engine == LocalAi` and `audio_mode == Surround` or `audio_enhance`: `LocalAi::open()` must succeed, else the export fails at once with that message.
  - `ExportEvent::Audio(AudioEvent)` forwarded; `Stage::Audio`.

- [ ] **Step 1: Failing tests:** `keeps_exact_frame_rate` (30000/1001 source, Basic provider, `audio_mode = Original` and again with `Surround`/`Classic`: output `r_frame_rate` == `30000/1001` exactly); `audio_matches_video_length` (2.5 s range of the 30000/1001 source with `Surround`/`Classic`: each audio stream's duration equals video duration within one frame); `surround_export_has_three_tracks` (MKV; stream 1 flac 6ch default, stream 2 ac3 6ch, stream 3 original codec); `audio_only_change_skips_frames` (second export with another `lfe_level` → no `FrameDone` events, video.mkv reused); `missing_pack_fails_before_frames` (LocalAi with no pack → `Failed` before any `Extracting` progress).
- [ ] **Step 2–4.** **Step 5:** Commit.

### Task 13: GUI

**Files:**
- Modify: `crates/upscaler-gui/src/ui/side_panel.rs` (new section "Ljud"), `ui/settings_window.rs` (new tab "Ljud-AI"), `app.rs` (export state for audio events, save dialog `.mkv` when `audio_active`), `worker.rs` (pack install/status workers)

**Interfaces:**
- Consumes: Tasks 1, 8, 11, 12.
- Side panel "Ljud" / "Audio": radio *Oförändrat / 5.1*; when 5.1: engine combo *AI (lokal, grafikkort)* (default) / *Klassisk (utan AI)*; LFE combo *Subtil / Normal / Kraftig / Biokänsla* (Biokänsla disabled with tooltip on Classic; whole combo disabled for 5.1 sources); checkbox *Förbättra ljudkvaliteten*; one status line: where it runs (GPU name or "processorn – cirka N tim per avsnitt"), output "MKV: FLAC 5.1 + AC-3 5.1 + original". Mono source: 5.1 disabled with reason. Pack missing: warning line with a button that opens the Ljud-AI tab.
- Settings tab "Ljud-AI": install / remove pack with progress and size, device line, "Testa" button (runs `device`).
- Export panel: audio stage name and `Section { done, total }` progress, warnings in yellow.

- [ ] **Step 1:** Implement; `cargo build -p upscaler-gui` clean, no warnings.
- [ ] **Step 2 (manual, release build):** open the Voyager file: Ljud section shows Dolby-flagged stereo; switch to 5.1 → engine defaults to AI; Biokänsla greys out on Klassisk; Ljud-AI tab installs the pack and shows the GPU; screenshot each state.
- [ ] **Step 3:** Commit.

### Task 14: Real-file verification and docs

**Files:**
- Modify: `README.md` (Swedish: new Ljud section, Ljud-AI install, formats, why MKV), `CLAUDE.md` (audio module layout, what is verified)

- [ ] **Step 1:** Export short ranges of the Voyager pilot with AI + Normal and with Classic: teaser (0:00–2:00), the bass event 01:00:25–01:00:45, a bridge dialogue scene, the theme. Record per run: time, GPU/CPU, warnings, loudness, sync result.
- [ ] **Step 2:** Same bass range with Biokänsla, and one range with enhancement only (stereo).
- [ ] **Step 3:** Write results into `CLAUDE.md` Status; update README.
- [ ] **Step 4:** `cargo test --workspace`, `cargo clippy --workspace` clean; commit.
- [ ] **Step 5:** Ask the owner to listen on the receiver (list the files and what to listen for).

## Spike findings

Task 8 Step 1, measured 2026-11-01 on the owner's PC (Windows 11 Pro 26200, RX 7900 XTX 24 GB,
Adrenalin driver 32.0.31041.1004 of 2026-08-17, 20 logical CPU cores).

- **ROCm PyTorch on Windows works on the 7900 XTX:** `torch 2.9.1+rocm7.2.1`,
  `torch.cuda.is_available() == True`, `get_device_name(0) == "AMD Radeon RX 7900 XTX"`. No
  WSL2, no DirectML.
- **Python must be 3.12, not 3.11:** AMD's wheels are `cp312` only. Embeddable
  `https://www.python.org/ftp/python/3.12.10/python-3.12.10-embed-amd64.zip` (3.12.10 is the last
  3.12 release with Windows binaries); `python312._pth` gets `import site`; pip from
  `https://bootstrap.pypa.io/get-pip.py`.
- **AMD wheels** (newest release on `https://repo.radeon.com/rocm/windows/`, docs
  `https://rocm.docs.amd.com/projects/radeon-ryzen/en/latest/docs/install/installrad/windows/install-pytorch.html`,
  needs driver 26.2.2 or newer), base `https://repo.radeon.com/rocm/windows/rocm-rel-7.2.1/`:
  1. SDK: `rocm_sdk_core-7.2.1-py3-none-win_amd64.whl` (645 MB),
     `rocm_sdk_libraries_custom-7.2.1-py3-none-win_amd64.whl`, `rocm-7.2.1.tar.gz` (an sdist: needs
     `setuptools` first, which the embeddable Python lacks). AMD's guide also lists
     `rocm_sdk_devel-7.2.1-py3-none-win_amd64.whl` (233 MB, 1.2 GB unpacked); torch runs without it
     (GPU separation verified after uninstalling it), so the pack skips it.
  2. Then `torch-2.9.1%2Brocm7.2.1-cp312-cp312-win_amd64.whl` (821 MB) and
     `torchaudio-2.9.1%2Brocm7.2.1-cp312-cp312-win_amd64.whl` (torchvision not needed).
  Download ~2 GB at ~20 MB/s here; pip install of SDK + torch ~3.5 min.
- **CPU fallback:** `torch==2.9.1 torchaudio==2.9.1` from `https://download.pytorch.org/whl/cpu`
  (`torch-2.9.1+cpu-cp312-cp312-win_amd64.whl` exists there).
- **MSST** pinned to commit `9cc76c03ccd4da5ddf3477e93d386a1648a77732` (main, 2026-10-30; latest tag
  v1.0.22): `https://github.com/ZFTurbo/Music-Source-Separation-Training/archive/9cc76c03ccd4da5ddf3477e93d386a1648a77732.zip`.
  Its `requirements.txt` is mostly training tools and does not install on Windows without a C
  compiler (`asteroid` → `pb-bss-eval` → `pesq`). What BandIt + Apollo inference imports, with
  `torch==2.9.1`/`torchaudio==2.9.1` as pip constraints so the ROCm build is kept: numpy, scipy,
  soundfile, soxr, librosa, PyYAML, omegaconf 2.2.3, ml_collections, tqdm, einops 0.8.1, beartype
  0.14.1, rotary_embedding_torch 0.3.5, pytorch_lightning, torchmetrics 0.11.4, spafe 0.3.2,
  pyloudnorm, pedalboard, torch_audiomentations, pandas, fire, asteroid-filterbanks,
  huggingface_hub, torch-optimizer, torch-stoi, **setuptools<81** (torchmetrics 0.11 imports
  `pkg_resources`), then `asteroid==0.7.0 --no-deps`. Exact versions are constants in `audio/pack.rs`.
- **MSST on Windows torch:** `torch.distributed` has no `is_initialized` in the Windows builds and
  MSST's `demix()` calls it; the worker stubs it.
- **Models** (URLs from MSST `docs/pretrained_models.md`):
  `https://github.com/ZFTurbo/Music-Source-Separation-Training/releases/download/v.1.0.3/config_dnr_bandit_bsrnn_multi_mus64.yaml`,
  `https://github.com/ZFTurbo/Music-Source-Separation-Training/releases/download/v.1.0.3/model_bandit_plus_dnr_sdr_11.47.chpt`
  (149 MB), Apollo `https://huggingface.co/JusperLee/Apollo/resolve/main/pytorch_model.bin` (67 MB) with
  MSST's `configs/config_apollo.yaml`. Both models run at 44.1 kHz; the worker resamples 48k → 44.1k
  → 48k (soxr_hq) and returns exactly the input's length.
- **Speed, BandIt Plus on the 120 s teaser** (`sample_teaser_0-120s.wav`, stereo 48 kHz), whole
  worker run including Python start and model load (~10 s):
  - GPU: 46.8 s first run, 42.8 s second (37.7 s through the installed pack and the Rust runner)
    → **~19–21 s per minute of audio** (~3× realtime), ~16 s/min excluding start-up. A 45-min
    episode ≈ 15 min.
  - CPU with the official CPU wheel (`torch 2.9.1+cpu`, the installer's fallback), 30 s excerpt:
    125 s → **~250 s (4.2 min) per minute of audio** (0.24× realtime); a 45-min episode ≈ 3 h.
  - CPU with the *ROCm* torch build (`AUDIO_AI_FORCE_CPU=1`): unusable, 657 s for half of the
    30 s excerpt (~43 min per audio minute); the 120 s run was stopped after 47 min. So a GPU
    machine must not fall back to the CPU inside the ROCm pack; the CPU path needs the CPU wheel.
  - Apollo (`enhance`) on GPU: 84 s for the 120 s teaser (~42 s per audio minute). On this AC-3
    source it does not add content above 14 kHz (14–20 kHz came out ~3 dB lower than the source);
    worth checking by ear in Task 10 before relying on it for band-splice.
- **Pack through the app's installer** (`examples/audio_pack -- install`, from empty): 7.3 min,
  **5.7 GB** on disk (ROCm SDK libraries ~3.4 GB, torch 1.2 GB), above the 2–5 GB estimate.
  `remove` of the pack took 4 min (many small files): the GUI should run it in the background.
- **Separation sanity (GPU):** speech −33.9, music −31.8, effects −32.1 dBFS; speech silent for the
  music-only first 20 s; source − sum of stems is 31.6 dB below the source (stems add back up).
