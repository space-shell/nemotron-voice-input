//! Native backend for `VoiceRecognitionService`, the `android.speech.RecognitionService`
//! implementation that lets *other* keyboards/apps (SwiftKey, Gboard, …) use this app
//! as their offline speech-to-text provider via the system `SpeechRecognizer` API.
//!
//! Unlike the IME / `RecognizeActivity` surfaces (which have their own UI and a manual
//! "tap to stop" control via `voice_session`), a `RecognitionService` has no UI of its
//! own: the calling keyboard expects *us* to decide when the user has finished speaking.
//! So this module adds trailing-silence endpointing on top of the same streaming
//! pipeline (`voice_session::run_stream_consumer`): audio is fed to the model live,
//! partial hypotheses are delivered as `partialResults`, and finalisation (explicit
//! `stopListening`, silence, or no-speech timeout) just ends the stream. The previous
//! 60 s hard cap is gone — the streaming model has constant memory.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use jni::objects::{GlobalRef, JClass, JObject};
use jni::JNIEnv;
use once_cell::sync::Lazy;

use crate::engine;
use crate::voice_session::{run_stream_consumer, SendStream};

// --- Endpointing / VAD tuning -------------------------------------------------
// These are deliberately simple heuristics on the smoothed mic level. Mic gain
// varies a lot between devices, so they may need tuning; finalisation always
// transcribes whatever was captured, so a mis-tuned threshold only affects the
// auto-stop *timing*, never whether text is returned.
//
/// Absolute smoothed level (0..1) that must be exceeded to count as speech.
const MIN_SPEECH_LEVEL: f32 = 0.12;
/// How far above the running noise floor a level must be to count as speech.
const SPEECH_MARGIN: f32 = 0.08;
/// Trailing silence after speech that triggers auto-finalisation.
const SILENCE_MS: u64 = 1500;
/// If no speech is ever detected, finalise after this long anyway.
const NO_SPEECH_TIMEOUT_MS: u64 = 7000;
/// Throttle interval for `rmsChanged` UI callbacks.
const LEVEL_UPDATE_MS: u64 = 50;
/// Grace period before a finalized-but-undelivered recognition is reported
/// as a stall error instead of leaving the caller waiting forever (#16).
const STALL_DELIVERY_MS: u64 = 30_000;

// Mirror of android.speech.SpeechRecognizer error codes we report.
const ERROR_AUDIO: i32 = 3;
const ERROR_SERVER: i32 = 4;
const ERROR_NO_MATCH: i32 = 7;

/// State shared between the audio callback, the endpoint-monitor thread and
/// the finaliser. Deliberately does NOT hold the cpal stream or the channel
/// sender, to avoid cycles (the stream's callback holds an `Arc<Endpoint>`).
struct Endpoint {
    last_voice: Mutex<Instant>,
    noise_floor: Mutex<f32>,
    last_level_sent: Mutex<Instant>,
    speech_started: AtomicBool,
    finalized: AtomicBool,
    /// Set when the consumer's delivery closure ran at all — the stall
    /// watchdog's off-switch (a stale-swallowed delivery still counts).
    delivered: AtomicBool,
    started_at: Instant,
    jvm: Arc<jni::JavaVM>,
    target: GlobalRef,
}

struct Session {
    shared: Arc<Endpoint>,
    stream: Arc<Mutex<Option<SendStream>>>,
    tx: Arc<Mutex<Option<crossbeam_channel::Sender<Vec<f32>>>>>,
    cancelled: Arc<AtomicBool>,
}

static SESSION: Lazy<Mutex<Option<Session>>> = Lazy::new(|| Mutex::new(None));

// --- JNI callbacks into VoiceRecognitionService -------------------------------

fn call_void(env: &mut JNIEnv, obj: &JObject, method: &str) {
    let _ = env.call_method(obj, method, "()V", &[]);
}

fn call_rms(env: &mut JNIEnv, obj: &JObject, rms_db: f32) {
    let _ = env.call_method(obj, "onRmsChanged", "(F)V", &[rms_db.into()]);
}

fn call_error(env: &mut JNIEnv, obj: &JObject, code: i32) {
    let _ = env.call_method(obj, "onError", "(I)V", &[code.into()]);
}

fn call_results(env: &mut JNIEnv, obj: &JObject, text: &str) {
    if let Ok(jtxt) = env.new_string(text) {
        let _ = env.call_method(
            obj,
            "onResults",
            "(Ljava/lang/String;)V",
            &[(&jtxt).into()],
        );
    }
}

// --- JNI entry points ---------------------------------------------------------

/// Called from `onCreate`. Warms up the model in the background so the first
/// recognition after a cold bind is as fast as possible.
#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_VoiceRecognitionService_initNative(
    env: JNIEnv,
    _class: JClass,
    service: JObject,
) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );

    let jvm = match env.get_java_vm() {
        Ok(vm) => Arc::new(vm),
        Err(_) => return,
    };
    let target_ref = match env.new_global_ref(&service) {
        Ok(r) => r,
        Err(_) => return,
    };

    std::thread::spawn(move || {
        let _ = engine::ensure_loaded_from_thread(&jvm, &target_ref);
    });
}

/// Called from `onStartListening`. Begins microphone capture, starts the
/// streaming consumer, and arms the silence-based endpoint monitor.
#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_VoiceRecognitionService_startListening(
    env: JNIEnv,
    _class: JClass,
    service: JObject,
) {
    let jvm = match env.get_java_vm() {
        Ok(vm) => Arc::new(vm),
        Err(_) => return,
    };
    let target = match env.new_global_ref(&service) {
        Ok(r) => r,
        Err(_) => return,
    };

    // Tear down any session that is still around (e.g. the keyboard called
    // startListening twice without cancel) so its monitor/consumer can never
    // deliver stale results to this new session.
    {
        let mut guard = SESSION.lock().unwrap();
        if let Some(old) = guard.take() {
            old.cancelled.store(true, Ordering::SeqCst);
            old.shared.finalized.store(true, Ordering::SeqCst);
            *old.stream.lock().unwrap() = None;
            *old.tx.lock().unwrap() = None;
        }
    }

    let now = Instant::now();
    let shared = Arc::new(Endpoint {
        last_voice: Mutex::new(now),
        noise_floor: Mutex::new(0.0),
        last_level_sent: Mutex::new(now),
        speech_started: AtomicBool::new(false),
        finalized: AtomicBool::new(false),
        delivered: AtomicBool::new(false),
        started_at: now,
        jvm: jvm.clone(),
        target,
    });
    let stream_holder: Arc<Mutex<Option<SendStream>>> = Arc::new(Mutex::new(None));
    let cancelled = Arc::new(AtomicBool::new(false));
    let overflow = Arc::new(AtomicBool::new(false));

    // Tell the keyboard we're ready to receive speech.
    {
        let mut env2 = match jvm.attach_current_thread() {
            Ok(e) => e,
            Err(_) => return,
        };
        call_void(&mut env2, shared.target.as_obj(), "onReadyForSpeech");
    }

    // Open the microphone (16 kHz mono, matching the model + voice_session).
    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            let mut env2 = jvm.attach_current_thread().unwrap();
            call_error(&mut env2, shared.target.as_obj(), ERROR_AUDIO);
            return;
        }
    };
    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(16000),
        buffer_size: cpal::BufferSize::Default,
    };

    // Capture → consumer channel. The consumer (run_stream_consumer) owns
    // the streaming session; dropping the sender finalises it.
    let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(64);
    let tx_holder: Arc<Mutex<Option<crossbeam_channel::Sender<Vec<f32>>>>> =
        Arc::new(Mutex::new(Some(tx.clone())));

    let cb_shared = shared.clone();
    let cb_overflow = overflow.clone();
    let cb_tx = tx;
    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _: &_| audio_callback(&cb_shared, &cb_tx, &cb_overflow, data),
        |e| log::error!("RecognitionService stream error: {}", e),
        None,
    );

    match stream {
        Ok(s) => {
            // Install the session before anything can observe the stream:
            // a stop or cancel arriving between play() and installation
            // would otherwise find no target to act on (#1).
            *SESSION.lock().unwrap() = Some(Session {
                shared: shared.clone(),
                stream: stream_holder.clone(),
                tx: tx_holder.clone(),
                cancelled: cancelled.clone(),
            });

            // Deliver the stream outcome as RecognitionService callbacks.
            // run_stream_consumer blocks for the whole recording, and this
            // JNI entry runs on the service's main looper — the consumer
            // MUST run on its own thread. Inline, main froze forever in the
            // recv_timeout loop: the mic never played (s.play() sat below
            // the call), SESSION was never installed, and the service ANR'd
            // (#1).
            let deliver_shared = shared.clone();
            let deliver_cancelled = cancelled.clone();
            let deliver_stream = stream_holder.clone();
            let deliver_tx = tx_holder.clone();
            let deliver_overflow = overflow.clone();
            std::thread::spawn(move || {
                run_stream_consumer(
                    jvm.clone(),
                    deliver_shared.target.clone(),
                    rx,
                    deliver_cancelled.clone(),
                    deliver_overflow,
                    move |env, obj, result| {
                        // The consumer reached delivery at all — clears the stall
                        // watchdog (#16) even when the result is swallowed as
                        // stale below.
                        deliver_shared.delivered.store(true, Ordering::SeqCst);
                        // Swallow everything if a newer session took over or this
                        // one was cancelled — no stale callbacks. (Note: the
                        // `finalized` flag is already true here — finalize() sets
                        // it before dropping the sender — so it is not a valid
                        // staleness signal for delivery.)
                        if deliver_cancelled.load(Ordering::SeqCst)
                            || !is_current_session(&deliver_shared)
                        {
                            return;
                        }
                        match result {
                            Ok(text) if !text.is_empty() => {
                                if deliver_shared.speech_started.load(Ordering::SeqCst) {
                                    call_void(env, obj, "onEndOfSpeech");
                                }
                                call_results(env, obj, &text);
                            }
                            Ok(_) => call_error(env, obj, ERROR_NO_MATCH),
                            Err(e) => {
                                log::error!("streaming recognition failed: {}", e);
                                // The consumer is gone — nothing will ever
                                // drain the audio. Stop capture too, or the
                                // mic keeps running (indicator on, audio
                                // discarded) until endpointing fires (#13).
                                *deliver_stream.lock().unwrap() = None;
                                *deliver_tx.lock().unwrap() = None;
                                call_error(env, obj, ERROR_SERVER);
                            }
                        }
                        clear_session(&deliver_shared);
                    },
                );
            });

            s.play().ok();
            *stream_holder.lock().unwrap() = Some(SendStream(s));
        }
        Err(e) => {
            log::error!("Failed to open microphone: {}", e);
            *tx_holder.lock().unwrap() = None;
            let mut env2 = jvm.attach_current_thread().unwrap();
            call_error(&mut env2, shared.target.as_obj(), ERROR_AUDIO);
            return;
        }
    }

    // Endpoint monitor.
    let mon_shared = shared.clone();
    let mon_stream = stream_holder.clone();
    let mon_tx = tx_holder.clone();
    let mon_overflow = overflow.clone();
    std::thread::spawn(move || {
        endpoint_monitor(mon_shared, mon_stream, mon_tx, mon_overflow)
    });
}

/// Called from `onStopListening`: the keyboard asked us to finish now. Finalise
/// with whatever the stream has consumed so far.
#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_VoiceRecognitionService_stopListening(
    _env: JNIEnv,
    _class: JClass,
) {
    let session = SESSION
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| (s.shared.clone(), s.stream.clone(), s.tx.clone()));
    if let Some((shared, stream, tx)) = session {
        std::thread::spawn(move || finalize(shared, stream, tx));
    }
}

/// Called from `onCancel`: discard everything, return nothing.
#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_VoiceRecognitionService_cancelNative(
    _env: JNIEnv,
    _class: JClass,
) {
    let mut guard = SESSION.lock().unwrap();
    if let Some(session) = guard.as_ref() {
        session.cancelled.store(true, Ordering::SeqCst);
        session.shared.finalized.store(true, Ordering::SeqCst);
        *session.stream.lock().unwrap() = None;
        *session.tx.lock().unwrap() = None;
    }
    *guard = None;
}

/// Called from `onDestroy`.
#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_VoiceRecognitionService_destroyNative(
    env: JNIEnv,
    class: JClass,
) {
    Java_dev_jamesnicholls_nemotronvoice_VoiceRecognitionService_cancelNative(env, class);
}

// --- Audio + endpointing ------------------------------------------------------

fn audio_callback(
    shared: &Arc<Endpoint>,
    tx: &crossbeam_channel::Sender<Vec<f32>>,
    overflow: &AtomicBool,
    data: &[f32],
) {
    if shared.finalized.load(Ordering::SeqCst) {
        return;
    }

    // Never block the realtime callback; overflow is handled by the monitor.
    if tx.try_send(data.to_vec()).is_err() {
        overflow.store(true, Ordering::SeqCst);
    }

    // RMS -> smoothed level in 0..1 (same scaling as voice_session).
    let mut sum = 0.0f32;
    for &x in data {
        sum += x * x;
    }
    let rms = (sum / (data.len().max(1) as f32)).sqrt();
    let level = (rms * 6.0).clamp(0.0, 1.0);

    let floor = *shared.noise_floor.lock().unwrap();
    let is_speech = level > MIN_SPEECH_LEVEL && level > floor + SPEECH_MARGIN;

    if is_speech {
        *shared.last_voice.lock().unwrap() = Instant::now();
        // First detected speech -> notify beginningOfSpeech exactly once.
        if shared
            .speech_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            if let Ok(mut env) = shared.jvm.attach_current_thread() {
                call_void(&mut env, shared.target.as_obj(), "onBeginningOfSpeech");
            }
        }
    } else {
        // Slowly adapt the noise floor while no speech is present.
        let mut nf = shared.noise_floor.lock().unwrap();
        *nf = *nf * 0.95 + level * 0.05;
    }

    // Throttled mic-level updates for the keyboard's waveform UI.
    let mut last = shared.last_level_sent.lock().unwrap();
    if last.elapsed() >= Duration::from_millis(LEVEL_UPDATE_MS) {
        *last = Instant::now();
        drop(last);
        if let Ok(mut env) = shared.jvm.attach_current_thread() {
            call_rms(&mut env, shared.target.as_obj(), level * 10.0);
        }
    }
}

fn endpoint_monitor(
    shared: Arc<Endpoint>,
    stream: Arc<Mutex<Option<SendStream>>>,
    tx: Arc<Mutex<Option<crossbeam_channel::Sender<Vec<f32>>>>>,
    overflow: Arc<AtomicBool>,
) {
    loop {
        std::thread::sleep(Duration::from_millis(100));

        if shared.finalized.load(Ordering::SeqCst) {
            return;
        }

        let elapsed = shared.started_at.elapsed();
        let speech = shared.speech_started.load(Ordering::SeqCst);
        let silence = shared.last_voice.lock().unwrap().elapsed();

        let done = overflow.load(Ordering::SeqCst)
            || (speech && silence >= Duration::from_millis(SILENCE_MS))
            || (!speech && elapsed >= Duration::from_millis(NO_SPEECH_TIMEOUT_MS));

        if done {
            finalize(shared, stream, tx);
            return;
        }
    }
}

/// Stop capture and end the stream; the consumer's delivery closure turns the
/// final text into onResults/onError. Idempotent: only the first caller
/// (monitor or explicit stop) does the work.
fn finalize(
    shared: Arc<Endpoint>,
    stream: Arc<Mutex<Option<SendStream>>>,
    tx: Arc<Mutex<Option<crossbeam_channel::Sender<Vec<f32>>>>>,
) {
    if shared
        .finalized
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return; // already finalised/cancelled
    }

    // Stop the microphone (also drops the audio callback's sender clone) and
    // drop the state's sender: the consumer sees Disconnected → drain +
    // finalize → delivery closure fires with the final text.
    *stream.lock().unwrap() = None;
    *tx.lock().unwrap() = None;

    // Delivery watchdog (#16): if the consumer never delivers (wedged load
    // or native inference), the caller's UI would wait forever. Surface a
    // server error after a grace period instead. The wedged thread itself
    // cannot be saved — recovery remains an app restart — but the keyboard
    // gets a real error rather than an eternal "processing". Cancel and
    // session-takeover paths clear SESSION, so is_current_session gates
    // this off for them.
    let wd = shared.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(STALL_DELIVERY_MS));
        if !wd.delivered.load(Ordering::SeqCst) && is_current_session(&wd) {
            log::error!(
                "no recognition result after {}ms; reporting stall",
                STALL_DELIVERY_MS
            );
            if let Ok(mut env) = wd.jvm.attach_current_thread() {
                call_error(&mut env, wd.target.as_obj(), ERROR_SERVER);
            }
            clear_session(&wd);
        }
    });
}

/// True while `shared` is still the session installed in SESSION.
fn is_current_session(shared: &Arc<Endpoint>) -> bool {
    let guard = SESSION.lock().unwrap();
    match guard.as_ref() {
        Some(s) => Arc::ptr_eq(&s.shared, shared),
        None => false,
    }
}

/// Clear the global session, but only if it is still *this* session — a newer
/// `startListening` may already have installed a fresh one.
fn clear_session(shared: &Arc<Endpoint>) {
    let mut guard = SESSION.lock().unwrap();
    if let Some(s) = guard.as_ref() {
        if Arc::ptr_eq(&s.shared, shared) {
            *guard = None;
        }
    }
}
