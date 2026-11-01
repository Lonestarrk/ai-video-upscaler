"""Audio AI worker for AI Video Upscaler. The app writes this file into its AI audio pack
(next to python/, msst/ and models/) and runs it with the pack's Python.

Commands:
  device                -> prints {"device":"gpu","name":"..."} or {"device":"cpu"}
  separate IN OUT_DIR   -> OUT_DIR/speech.wav, music.wav, effects.wav (MSST BandIt Plus)
  enhance IN OUT        -> OUT (MSST Apollo, lossy-codec repair)

IN is a WAV file; outputs are 32-bit float WAVs with IN's sample rate, channel count and
exact length. Progress goes to stdout as "PROGRESS 0.42" lines; everything else (including
MSST's own prints) goes to stderr. Errors exit with a non-zero code.
Set AUDIO_AI_FORCE_CPU=1 to run on the CPU even when a GPU is available.
"""

import json
import os
import sys
import traceback

HERE = os.path.dirname(os.path.abspath(__file__))
MSST = os.path.join(HERE, "msst")
MODELS = os.path.join(HERE, "models")

SEPARATE_MODEL = ("bandit",
                  os.path.join(MODELS, "config_dnr_bandit_bsrnn_multi_mus64.yaml"),
                  os.path.join(MODELS, "model_bandit_plus_dnr_sdr_11.47.chpt"))
ENHANCE_MODEL = ("apollo",
                 os.path.join(MSST, "configs", "config_apollo.yaml"),
                 os.path.join(MODELS, "apollo_pytorch_model.bin"))

# Only our own answers go to the real stdout; library chatter goes to stderr.
OUT = sys.stdout
sys.stdout = sys.stderr


def emit(text):
    OUT.write(text + "\n")
    OUT.flush()


def progress(x):
    emit("PROGRESS %.4f" % max(0.0, min(1.0, x)))


def pick_device():
    import torch
    if os.environ.get("AUDIO_AI_FORCE_CPU") != "1" and torch.cuda.is_available():
        return "cuda:0", torch.cuda.get_device_name(0)
    return "cpu", None


def cmd_device():
    dev, name = pick_device()
    if dev == "cpu":
        emit(json.dumps({"device": "cpu"}))
    else:
        emit(json.dumps({"device": "gpu", "name": name}))


class Stage:
    """Maps MSST's progress bar onto a slice [lo, hi] of the overall progress."""
    lo = 0.0
    hi = 1.0


class ProgressBar:
    """Stands in for tqdm inside MSST's demix()."""

    def __init__(self, iterable=None, total=None, **_kwargs):
        self.iterable = iterable
        self.total = total if total else (len(iterable) if iterable is not None else 1)
        self.n = 0

    def __iter__(self):
        for item in self.iterable:
            yield item
            self.update(1)

    def update(self, k=1):
        self.n += k
        frac = min(1.0, self.n / max(1, self.total))
        progress(Stage.lo + (Stage.hi - Stage.lo) * frac)

    def close(self):
        pass


def load_model(spec, device):
    model_type, config_path, checkpoint = spec
    for path in (config_path, checkpoint):
        if not os.path.isfile(path):
            raise RuntimeError("missing model file: " + path)
    if MSST not in sys.path:
        sys.path.insert(0, MSST)
    import torch
    import torch.distributed as dist
    if not hasattr(dist, "is_initialized"):
        # Windows builds (ROCm and CPU) ship without torch.distributed; MSST asks it anyway.
        dist.is_initialized = lambda: False
        dist.get_rank = lambda: 0
    from utils.settings import get_model_from_config
    from utils import model_utils
    model_utils.tqdm = ProgressBar

    model, config = get_model_from_config(model_type, config_path)
    state = torch.load(checkpoint, map_location="cpu", weights_only=False)
    for key in ("state", "state_dict", "model_state_dict"):
        if isinstance(state, dict) and key in state:
            state = state[key]
    model.load_state_dict(state)
    model = model.to(device).eval()
    return model, config, model_utils


def read_wav(path):
    import soundfile as sf
    data, sr = sf.read(path, dtype="float32", always_2d=True)
    return data.T, sr  # (channels, frames)


def resample(x, sr_from, sr_to):
    if sr_from == sr_to:
        return x
    import librosa
    return librosa.resample(x, orig_sr=sr_from, target_sr=sr_to, res_type="soxr_hq", axis=-1)


def fit(x, n):
    """Pads with silence or trims (channels, frames) to exactly n frames."""
    import numpy as np
    if x.shape[-1] >= n:
        return x[..., :n]
    return np.pad(x, ((0, 0), (0, n - x.shape[-1])))


def write_wav(path, x, sr):
    import numpy as np
    import soundfile as sf
    part = path + ".part"
    sf.write(part, np.ascontiguousarray(x.T, dtype=np.float32), sr, subtype="FLOAT", format="WAV")
    os.replace(part, path)


def run_model(spec, inp):
    """Runs one MSST model on `inp`; returns ({instrument: (channels, frames)}, sr)."""
    import numpy as np
    progress(0.0)
    device, _ = pick_device()
    model, config, model_utils = load_model(spec, device)
    progress(0.05)

    mix, sr = read_wav(inp)
    channels, frames = mix.shape
    if channels == 1:
        mix = np.concatenate([mix, mix], axis=0)
    elif channels != 2:
        raise RuntimeError("expected mono or stereo input, got %d channels" % channels)
    model_sr = int(getattr(config.audio, "sample_rate", 44100))
    work = np.ascontiguousarray(resample(mix, sr, model_sr), dtype=np.float32)

    Stage.lo, Stage.hi = 0.05, 0.95
    result = model_utils.demix(config, model, work, device, spec[0], pbar=True)
    if not isinstance(result, dict):
        result = {model_utils.prefer_target_instrument(config)[0]: result}

    out = {}
    for name, wave in result.items():
        wave = fit(resample(np.asarray(wave, dtype=np.float32), model_sr, sr), frames)
        if channels == 1:
            wave = wave.mean(axis=0, keepdims=True)
        out[name] = wave
    return out, sr


def cmd_separate(inp, out_dir):
    stems, sr = run_model(SEPARATE_MODEL, inp)
    os.makedirs(out_dir, exist_ok=True)
    names = ("speech", "music", "effects")
    for i, name in enumerate(names):
        if name not in stems:
            raise RuntimeError("the model did not return a %s stem" % name)
        write_wav(os.path.join(out_dir, name + ".wav"), stems[name], sr)
        progress(0.95 + 0.05 * (i + 1) / len(names))


def cmd_enhance(inp, out):
    result, sr = run_model(ENHANCE_MODEL, inp)
    wave = result.get("restored")
    if wave is None:
        raise RuntimeError("the model did not return a restored track")
    parent = os.path.dirname(os.path.abspath(out))
    os.makedirs(parent, exist_ok=True)
    write_wav(out, wave, sr)
    progress(1.0)


def main(argv):
    if len(argv) == 1 and argv[0] == "device":
        cmd_device()
    elif len(argv) == 3 and argv[0] == "separate":
        cmd_separate(argv[1], argv[2])
    elif len(argv) == 3 and argv[0] == "enhance":
        cmd_enhance(argv[1], argv[2])
    else:
        sys.stderr.write("usage: audio_ai.py device | separate IN OUT_DIR | enhance IN OUT\n")
        return 2
    return 0


if __name__ == "__main__":
    try:
        code = main(sys.argv[1:])
    except Exception as e:  # noqa: BLE001 - every failure must reach the app as text
        traceback.print_exc(file=sys.stderr)
        sys.stderr.write("ERROR: %s\n" % e)
        code = 1
    sys.stderr.flush()
    sys.exit(code)
