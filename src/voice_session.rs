//! Streaming voice capture → live partial transcription.
//!
//! The cpal capture callback pushes 16 kHz mono chunks into a bounded
//! crossbeam channel; a consumer thread owns a dedicated transcribe.cpp
//! session + stream, feeds ~100 ms chunks as they arrive, and reports the
//! UI-stable committed prefix and the revisable tentative tail to Java via
//! `onPartialText`. `stopRecording` drops the channel sender, which the
//! consumer observes as disconnect → drain → finalize → single final
//! `onTextTranscribed`. Nothing blocks the realtime audio callback; if
//! inference falls behind the bounded channel fills and the session ends
//! with an explicit error instead of silently dropping audio.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use jni::objects::{GlobalRef, JObject};
use jni::JNIEnv;

use crate::engine;

// --- Optional auto-stop endpointing (same level heuristics as recog_service) --
/// Absolute smoothed level (0..1) that must be exceeded to count as speech.
const MIN_SPEECH_LEVEL: f32 = 0.12;
/// How far above the running noise floor a level must be to count as speech.
const SPEECH_MARGIN: f32 = 0.08;
/// Trailing silence after speech that triggers auto-stop.
const AUTO_STOP_SILENCE_MS: u64 = 2000;
/// If no speech is ever detected, auto-stop after this long.
const AUTO_STOP_NO_SPEECH_MS: u64 = 8000;

// --- Streaming pipeline tuning ----------------------------------------------
/// Audio fed to the model per `stream.feed` call: 100 ms at 16 kHz.
const FEED_CHUNK_SAMPLES: usize = 1600;
/// Channel capacity in chunks (~6.4 s of headroom). Inference on a phone-class
/// SoC runs several times faster than realtime; this only trips if the device
/// is thoroughly stalled, and the failure is loud rather than silent.
const CHANNEL_CHUNKS: usize = 64;
/// Cache-aware streaming lookahead: att_context_right = 1 (80 ms). First text
/// arrives ~0.16 s after speech starts; the accuracy cost vs the max-accuracy
/// setting is ~0.15% WER (transcribe.cpp streaming validation table).
const ATT_CONTEXT_RIGHT: i32 = 1;

pub struct SendStream(#[allow(dead_code)] pub cpal::Stream);
unsafe impl Send for SendStream {}
unsafe impl Sync for SendStream {}

/// Speech/silence tracking shared between the audio callback and the
/// auto-stop monitor thread.
struct Endpointing {
    last_voice: Mutex<Instant>,
    noise_floor: Mutex<f32>,
    speech_started: AtomicBool,
}

pub struct VoiceSessionState {
    pub stream: Option<SendStream>,
    pub jvm: Arc<jni::JavaVM>,
    pub target_ref: GlobalRef,
    pub last_level_sent: Arc<Mutex<std::time::Instant>>,
    /// True while the current recording runs; flipped off on stop/cancel so
    /// the auto-stop monitor (if any) exits.
    pub session_active: Arc<AtomicBool>,
    /// Sender half of the capture→consumer channel. Dropping it (on stop or
    /// cancel) signals the consumer to finalize (stop) or abandon (cancel).
    pub feed_tx: Option<crossbeam_channel::Sender<Vec<f32>>>,
    /// Set by the consumer path to abandon the stream without finalizing.
    pub cancelled: Arc<AtomicBool>,
    /// Set by the capture callback when the channel is full (consumer can't
    /// keep up). The session ends with an error.
    pub overflow: Arc<AtomicBool>,
}

fn notify_status(env: &mut JNIEnv, obj: &JObject, msg: &str) {
    if let Ok(jmsg) = env.new_string(msg) {
        let _ = env.call_method(
            obj,
            "onStatusUpdate",
            "(Ljava/lang/String;)V",
            &[(&jmsg).into()],
        );
    }
}

fn notify_level(env: &mut JNIEnv, obj: &JObject, level: f32) {
    let _ = env.call_method(obj, "onAudioLevel", "(F)V", &[level.into()]);
}

fn notify_text(env: &mut JNIEnv, obj: &JObject, text: &str) {
    if let Ok(jtxt) = env.new_string(text) {
        let _ = env.call_method(
            obj,
            "onTextTranscribed",
            "(Ljava/lang/String;)V",
            &[(&jtxt).into()],
        );
    }
}

/// Live partial: `committed` is the append-only UI-stable prefix, `tentative`
/// the still-revisable tail. Java keeps what it has already committed and
/// composes the tail.
fn notify_partial(env: &mut JNIEnv, obj: &JObject, committed: &str, tentative: &str) {
    if let (Ok(jc), Ok(jt)) = (env.new_string(committed), env.new_string(tentative)) {
        let _ = env.call_method(
            obj,
            "onPartialText",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[(&jc).into(), (&jt).into()],
        );
    }
}

pub fn init_session(env: JNIEnv, target: JObject) -> VoiceSessionState {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );

    let vm = env.get_java_vm().expect("Failed to get JavaVM");
    let vm_arc = Arc::new(vm);
    let target_ref = env.new_global_ref(&target).expect("Failed to ref target");

    let state = VoiceSessionState {
        stream: None,
        jvm: vm_arc.clone(),
        target_ref: target_ref.clone(),
        last_level_sent: Arc::new(Mutex::new(std::time::Instant::now())),
        session_active: Arc::new(AtomicBool::new(false)),
        feed_tx: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        overflow: Arc::new(AtomicBool::new(false)),
    };

    // Load engine in background
    let vm_clone = vm_arc.clone();
    let target_ref_clone = target_ref.clone();

    std::thread::spawn(move || {
        let _ = engine::ensure_loaded_from_thread(&vm_clone, &target_ref_clone);
    });

    state
}

/// Begin microphone capture and start the streaming consumer thread. With
/// `auto_stop` set, a monitor thread watches for trailing silence after
/// speech (or a no-speech timeout) and invokes the Java-side `onAutoStop()`
/// callback, which is expected to stop the recording the same way a manual
/// tap would.
pub fn start_recording(mut env: JNIEnv, state: &mut VoiceSessionState, auto_stop: bool) {
    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            notify_status(
                &mut env,
                state.target_ref.as_obj(),
                "Error: no microphone available. Check permissions.",
            );
            return;
        }
    };

    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(16000),
        buffer_size: cpal::BufferSize::Default,
    };

    // End any previous session's monitor, then arm a fresh flag.
    state.session_active.store(false, Ordering::SeqCst);
    let session_active = Arc::new(AtomicBool::new(true));
    state.session_active = session_active.clone();
    state.cancelled = Arc::new(AtomicBool::new(false));
    state.overflow = Arc::new(AtomicBool::new(false));

    let endpoint = if auto_stop {
        Some(Arc::new(Endpointing {
            last_voice: Mutex::new(Instant::now()),
            noise_floor: Mutex::new(0.0),
            speech_started: AtomicBool::new(false),
        }))
    } else {
        None
    };

    let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(CHANNEL_CHUNKS);
    state.feed_tx = Some(tx.clone());
    let overflow = state.overflow.clone();

    let jvm = state.jvm.clone();
    let target_ref = state.target_ref.clone();
    let last_sent = state.last_level_sent.clone();
    let endpoint_cb = endpoint.clone();

    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _: &_| {
            // Never block the realtime callback: if the consumer is hopelessly
            // behind, flag overflow and let it end the session loudly.
            if tx.try_send(data.to_vec()).is_err() {
                overflow.store(true, Ordering::SeqCst);
            }

            // compute RMS
            let mut sum = 0.0f32;
            for &x in data {
                sum += x * x;
            }
            let rms = (sum / (data.len().max(1) as f32)).sqrt();
            let level = (rms * 6.0).clamp(0.0, 1.0);

            if let Some(ep) = &endpoint_cb {
                let floor = *ep.noise_floor.lock().unwrap();
                let is_speech = level > MIN_SPEECH_LEVEL && level > floor + SPEECH_MARGIN;
                if is_speech {
                    *ep.last_voice.lock().unwrap() = Instant::now();
                    ep.speech_started.store(true, Ordering::SeqCst);
                } else {
                    // Slowly adapt the noise floor while no speech is present.
                    let mut nf = ep.noise_floor.lock().unwrap();
                    *nf = *nf * 0.95 + level * 0.05;
                }
            }

            // throttle updates
            let mut last = last_sent.lock().unwrap();
            if last.elapsed() >= std::time::Duration::from_millis(50) {
                *last = std::time::Instant::now();

                if let Ok(mut env) = jvm.attach_current_thread() {
                    let obj = target_ref.as_obj();
                    notify_level(&mut env, obj, level);
                }
            }
        },
        |e| log::error!("Stream err: {}", e),
        None,
    );

    match stream {
        Ok(s) => {
            // Start the consumer before the capture so no chunk is raced.
            let jvm = state.jvm.clone();
            let target_ref = state.target_ref.clone();
            let cancelled = state.cancelled.clone();
            let overflow = state.overflow.clone();
            std::thread::spawn(move || {
                run_stream_consumer(
                    jvm,
                    target_ref,
                    rx,
                    cancelled,
                    overflow,
                    |env, obj, result| match result {
                        Ok(text) => {
                            notify_status(env, obj, "Ready");
                            notify_text(env, obj, &text);
                        }
                        Err(msg) => notify_status(env, obj, &format!("Error: {}", msg)),
                    },
                );
            });

            s.play().ok();
            state.stream = Some(SendStream(s));
            notify_status(&mut env, state.target_ref.as_obj(), "Listening...");

            if let Some(ep) = endpoint {
                let jvm = state.jvm.clone();
                let target_ref = state.target_ref.clone();
                let started_at = Instant::now();
                std::thread::spawn(move || loop {
                    std::thread::sleep(Duration::from_millis(100));
                    if !session_active.load(Ordering::SeqCst) {
                        return;
                    }
                    let speech = ep.speech_started.load(Ordering::SeqCst);
                    let silence = ep.last_voice.lock().unwrap().elapsed();
                    let done = (speech
                        && silence >= Duration::from_millis(AUTO_STOP_SILENCE_MS))
                        || (!speech
                            && started_at.elapsed()
                                >= Duration::from_millis(AUTO_STOP_NO_SPEECH_MS));
                    if done {
                        // Claim the session so a simultaneous manual stop and
                        // this monitor can't both fire.
                        if session_active.swap(false, Ordering::SeqCst) {
                            if let Ok(mut env) = jvm.attach_current_thread() {
                                let _ = env.call_method(
                                    target_ref.as_obj(),
                                    "onAutoStop",
                                    "()V",
                                    &[],
                                );
                            }
                        }
                        return;
                    }
                });
            }
        }
        Err(e) => {
            state.feed_tx = None;
            notify_status(
                &mut env,
                state.target_ref.as_obj(),
                &format!("Error: failed to open microphone: {}", e),
            );
        }
    }
}

/// Owns a dedicated session + stream for one recording. Runs until the sender
/// is dropped (manual/auto stop → finalize) or the session is cancelled /
/// overflows (abandon). Partials go to `target` via `onPartialText`; the final
/// outcome goes to `deliver`, executed on this thread with an attached JNIEnv.
/// Same panic-hardening as `engine::transcribe_shared`: a panic anywhere in
/// the engine stack surfaces as a normal error instead of freezing every
/// later one.
///
/// Runs on the CALLING thread and blocks for the entire recording — callers
/// must invoke it on a dedicated thread. The name used to say "spawn", which
/// hid the blocking contract and led to the RecognitionService running it
/// inline on its main looper (#1).
pub fn run_stream_consumer<F>(
    jvm: Arc<jni::JavaVM>,
    target_ref: GlobalRef,
    rx: crossbeam_channel::Receiver<Vec<f32>>,
    cancelled: Arc<AtomicBool>,
    overflow: Arc<AtomicBool>,
    deliver: F,
) where
    F: FnOnce(&mut JNIEnv, &JObject, Result<String, String>) + Send + 'static,
{
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        stream_session_body(&jvm, &target_ref, rx, &cancelled, &overflow)
    }))
    .unwrap_or_else(|_| {
        log::error!("streaming consumer panicked; reporting as error");
        Err("transcription failed unexpectedly, please try again".to_string())
    });
    if let Ok(mut env) = jvm.attach_current_thread() {
        deliver(&mut env, target_ref.as_obj(), result);
    }
}

/// Runs one streaming session over the channel. Returns the final text
/// (sender dropped → drain + finalize), or `Ok(())`-without-text semantics
/// for cancellation: `Ok(String::new())` means cancelled/abandoned.
fn stream_session_body(
    jvm: &Arc<jni::JavaVM>,
    target_ref: &GlobalRef,
    rx: crossbeam_channel::Receiver<Vec<f32>>,
    cancelled: &Arc<AtomicBool>,
    overflow: &Arc<AtomicBool>,
) -> Result<String, String> {
    let mut env = jvm
        .attach_current_thread()
        .map_err(|_| "Failed to attach JNI thread".to_string())?;
    let obj = target_ref.as_obj();

    // Wait for the engine if it is still loading (audio buffers in the
    // channel meanwhile — bounded, so a slow load can still overflow).
    if engine::get_engine().is_none() {
        engine::ensure_loaded(&mut env, obj)?;
    }
    let engine_arc = engine::get_engine().ok_or("model not loaded")?;

    let mut session = {
        let guard = engine_arc
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.stream_session()?
    };
    let stream_opts = transcribe_cpp::StreamOptions {
        commit_policy: transcribe_cpp::CommitPolicy::Auto,
        family: Some(transcribe_cpp::StreamExtension::ParakeetStream(
            transcribe_cpp::ParakeetStreamOptions {
                att_context_right: Some(ATT_CONTEXT_RIGHT),
            },
        )),
        ..Default::default()
    };
    let mut stream = session
        .stream(&transcribe_cpp::RunOptions::default(), &stream_opts)
        .map_err(|e| e.to_string())?;

    let started = std::time::Instant::now();
    let mut received_samples: usize = 0;
    let mut buf: Vec<f32> = Vec::new();
    let mut last_partial: Option<(String, String)> = None;

    loop {
        // Cancel beats everything: abandon without finalizing.
        if cancelled.load(Ordering::SeqCst) {
            stream.reset();
            return Ok(String::new());
        }
        if overflow.load(Ordering::SeqCst) {
            stream.reset();
            return Err("can't keep up: transcription fell behind live audio".to_string());
        }

        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                received_samples += chunk.len();
                buf.extend_from_slice(&chunk);
            }
            // A timeout just means quiet audio: keep looping (flags are
            // re-checked at the top). When every sender is dropped and the
            // channel is empty, recv returns Disconnected immediately.
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        while buf.len() >= FEED_CHUNK_SAMPLES {
            let chunk: Vec<f32> = buf.drain(..FEED_CHUNK_SAMPLES).collect();
            let update = stream.feed(&chunk).map_err(|e| e.to_string())?;
            if update.committed_changed || update.tentative_changed {
                let text = stream.text();
                let changed = last_partial
                    .as_ref()
                    .map(|(c, t)| c != &text.committed || t != &text.tentative)
                    .unwrap_or(true);
                if changed {
                    last_partial = Some((text.committed.clone(), text.tentative.clone()));
                    notify_partial(&mut env, obj, &text.committed, &text.tentative);
                }
            }
        }
    }

    // Final flush: feed any sub-chunk remainder, then finalize. The model's
    // finalize emits a final partial chunk that produces closing punctuation.
    if !buf.is_empty() {
        let remainder = std::mem::take(&mut buf);
        let _ = stream.feed(&remainder);
    }
    let _update = stream.finalize().map_err(|e| e.to_string())?;
    let text = stream.text();
    let final_text = text.full.trim().to_string();

    let audio_secs = received_samples as f64 / 16_000.0;
    log::info!(
        "streamed {:.1}s audio in {:.1}s wall clock, final: {:?}",
        audio_secs,
        started.elapsed().as_secs_f64(),
        final_text
    );

    Ok(final_text)
}

pub fn stop_recording(mut env: JNIEnv, state: &mut VoiceSessionState) {
    // Drop the stream to stop recording; end the auto-stop monitor if running.
    state.session_active.store(false, Ordering::SeqCst);
    state.stream = None;
    // Dropping the sender signals the consumer to drain + finalize. The final
    // text arrives via onTextTranscribed from the consumer thread.
    state.feed_tx = None;
    notify_status(&mut env, state.target_ref.as_obj(), "Finishing...");
}

pub fn cancel_recording(mut env: JNIEnv, state: &mut VoiceSessionState) {
    state.cancelled.store(true, Ordering::SeqCst);
    state.session_active.store(false, Ordering::SeqCst);
    state.stream = None;
    state.feed_tx = None;
    notify_status(&mut env, state.target_ref.as_obj(), "Canceled");
}
