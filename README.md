# Nemotron Voice Input (Android)

Offline, privacy-focused **streaming** speech-to-text for Android, built with
Rust. Speak into any text field and words appear **live while you speak** —
transcribed entirely on-device by NVIDIA's Nemotron speech streaming model.
Fork of [notune/android_transcribe_app](https://github.com/notune/android_transcribe_app)
("Offline Voice Input"), reworked around true incremental transcription.

## What changed vs upstream

- **Live streaming text**: audio is fed to the model in ~100 ms chunks through
  transcribe.cpp's cache-aware streaming API (`att_context_right = 1`, ~80 ms
  lookahead — first text lands ~0.16 s after you start speaking). Stable text
  is committed to the input field as it firms up; the still-revisable tail
  shows as the composing region. No more record → wait → paste.
- **Single model, no model management**: the bundled (and only) model is
  `nvidia/nemotron-speech-streaming-en-0.6b` (Q8_0, ~700 MB, English, natively
  cased + punctuated). The model-import screen, language picker and translate
  toggle are gone.
- **Streaming everywhere**: the IME, the voice-input popup, the system
  `RecognitionService` (used by other keyboards via `SpeechRecognizer`, now
  with `partialResults`), and live subtitles all run on the same pipeline.
  The old 60 s dictation cap is gone — the streaming model has constant memory.
- **Return to previous keyboard** after a successful transcription (on by
  default, toggleable) — closes the loop of "keyboard disappears after
  dictation" (upstream #63).
- **Floating keyboard mode**: the voice keyboard becomes a movable panel with
  a drag handle — on by default for tablet-class screens (upstream request).
- **Hold-to-cancel**: long-press the record button to discard a dictation
  (upstream #66), plus the theme-change insets fix (upstream #99 / PR #100).

## Usage

Same integration surface as upstream:

| Path | Who uses it | What happens |
|---|---|---|
| **Voice-input popup** (`RECOGNIZE_SPEECH`) | SwiftKey, website voice search | Compact panel with live text over the current app |
| **System speech service** (`RecognitionService`) | Keyboards/apps using `SpeechRecognizer` | Streaming recognition in the background with partial results |
| **Voice keyboard (IME)** | Any keyboard switcher | Dedicated streaming voice keyboard |

## Building

Everything needed is in the Nix flake (JDK 17, Android SDK 35 + NDK 28,
Rust with the `aarch64-linux-android` target, cargo-ndk, cmake):

```bash
nix develop -c ./gradlew assembleDebug
# Output: app/build/outputs/apk/debug/app-debug.apk
```

The first build downloads the ~700 MB Nemotron GGUF from Hugging Face and
cross-compiles Rust + ggml (~5–10 min). Release builds expect
`release.keystore` in the project root and `KEY_ALIAS` / `KEY_PASS` /
`STORE_PASS` in the environment.

Without Nix you need: JDK 17, Android SDK + NDK 28.0.13004108, Rust +
`aarch64-linux-android` target, `cargo install cargo-ndk`, cmake.

## Project structure

- `src/` — Rust core (cdylib): engine, streaming voice session, JNI bridges
- `app/src/main/java/dev/jamesnicholls/nemotronvoice/` — Android Java
- `model_assets/` — Play Asset Delivery pack for the bundled GGUF
- Transcription runs through
  [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp) (MIT).

## Licenses & attribution

- App code: MIT (© notune, modifications © this fork)
- [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp): MIT
- Model: [`nvidia/nemotron-speech-streaming-en-0.6b`](https://huggingface.co/nvidia/nemotron-speech-streaming-en-0.6b)
  — [NVIDIA Open Model License](https://www.nvidia.com/en-us/agreements/enterprise-software/nvidia-open-model-license/)
  (GGUF conversion by [handy-computer](https://huggingface.co/handy-computer))
