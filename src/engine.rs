//! Loads and serves the speech-to-text engine (transcribe.cpp, GGUF models).
//!
//! The engine is a process-wide singleton behind `Arc<Mutex<..>>`: a
//! transcribe.cpp session may only be used by one thread at a time, which the
//! mutex guarantees. Exactly one model exists — the Nemotron speech streaming
//! GGUF bundled in the APK assets — so there is no selection state.

use once_cell::sync::Lazy;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use jni::objects::{GlobalRef, JObject};
use jni::JNIEnv;

use crate::assets;

/// Optional file in filesDir with the CPU thread count for inference.
/// Absent/invalid/0 = default (performance-core heuristic).
const MODEL_THREADS_FILE: &str = "model_threads";

/// A loaded transcribe.cpp model plus the batch session used by one-shot
/// transcription (benchmark, subtitles, file transcription). Streaming runs
/// get their own session via [`Engine::stream_session`]: `transcribe_cpp`
/// streams mutably borrow their session, and a session is `Send`, so handing
/// out per-run sessions keeps the stream owned by its consumer thread without
/// holding the shared-engine mutex for the stream's lifetime. The bindings'
/// per-model compute lease still serializes native compute across sessions.
pub struct Engine {
    model: transcribe_cpp::Model,
    session: transcribe_cpp::Session,
    session_options: transcribe_cpp::SessionOptions,
}

impl Engine {
    fn load(model_path: &Path, threads: i32) -> Result<Engine, String> {
        if !model_path.is_file() {
            return Err(format!("model file not found: {}", model_path.display()));
        }
        let model = transcribe_cpp::Model::load(model_path).map_err(|e| e.to_string())?;
        log::info!("engine: {} threads", threads);
        let session_options = transcribe_cpp::SessionOptions {
            n_threads: threads,
            ..Default::default()
        };
        let session = model
            .session_with(&session_options)
            .map_err(|e| e.to_string())?;
        Ok(Engine {
            model,
            session,
            session_options,
        })
    }

    /// Transcribes 16 kHz mono f32 samples to text in one pass. The Nemotron
    /// streaming model has no context/KV ceiling, so audio of any length is
    /// processed in a single run.
    pub fn transcribe(&mut self, samples: Vec<f32>) -> Result<String, String> {
        let opts = transcribe_cpp::RunOptions::default();
        self.session
            .run(&samples, &opts)
            .map(|t| t.text)
            .map_err(|e| e.to_string())
    }

    /// A fresh session for a streaming run, to be owned (and used) by one
    /// thread. While a stream on it is active, batch `run`s on other sessions
    /// of the same model fail with `Busy` — the C library permits one
    /// in-flight compute per model.
    pub fn stream_session(&self) -> Result<transcribe_cpp::Session, String> {
        self.model
            .session_with(&self.session_options)
            .map_err(|e| e.to_string())
    }
}

/// Holds the loaded engine singleton.
static GLOBAL_ENGINE: Lazy<Mutex<Option<Arc<Mutex<Engine>>>>> =
    Lazy::new(|| Mutex::new(None));

/// Loading coordination state + condvar for waiters.
static LOAD_STATE: Lazy<(Mutex<LoadState>, Condvar)> =
    Lazy::new(|| (Mutex::new(LoadState::Idle), Condvar::new()));

#[derive(Debug, Clone, PartialEq)]
enum LoadState {
    /// No load in progress
    Idle,
    /// A thread is currently loading the model
    Loading,
    /// Loading completed successfully
    Done,
    /// Loading failed
    Failed(String),
}

pub fn get_engine() -> Option<Arc<Mutex<Engine>>> {
    GLOBAL_ENGINE.lock().unwrap().clone()
}

/// Runs a transcription on the shared engine. Two layers of hardening keep a
/// single bad run from freezing every later one (the symptom would be an IME
/// stuck at "Processing" until its process dies, since notify callbacks stop
/// coming): a panic anywhere in the engine stack is caught and surfaced as a
/// normal error, and a lock poisoned by an earlier panic is recovered instead
/// of propagating the poison forever.
pub fn transcribe_shared(engine: &Arc<Mutex<Engine>>, samples: Vec<f32>) -> Result<String, String> {
    let audio_secs = samples.len() as f64 / 16_000.0;
    let started = std::time::Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut guard = engine.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.transcribe(samples)
    }))
    .unwrap_or_else(|_| {
        log::error!("transcription panicked; reporting as error");
        Err("transcription failed unexpectedly, please try again".to_string())
    });
    log::info!(
        "transcribed {:.1}s audio in {:.2}s",
        audio_secs,
        started.elapsed().as_secs_f64()
    );
    result
}

pub fn is_engine_loaded() -> bool {
    GLOBAL_ENGINE.lock().unwrap().is_some()
}

/// Drops the loaded engine and clears the load state so the next
/// `ensure_loaded*` call reloads. Waits for an in-flight load to finish
/// first, so a reload can't race a load. Call from a background thread.
pub fn reset() {
    let (lock, cvar) = &*LOAD_STATE;
    let mut state = lock.lock().unwrap();
    while *state == LoadState::Loading {
        state = cvar.wait(state).unwrap();
    }
    *GLOBAL_ENGINE.lock().unwrap() = None;
    *state = LoadState::Idle;
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

/// Ensures the engine is loaded. Safe to call from multiple threads
/// concurrently; reports status via the target's `onStatusUpdate` callback.
///
/// Convenience wrapper around [`ensure_loaded_from_thread`] for JNI entry
/// points that already hold an attached `JNIEnv`.
pub fn ensure_loaded(env: &mut JNIEnv, context: &JObject) -> Result<(), String> {
    let jvm = Arc::new(env.get_java_vm().map_err(|e| e.to_string())?);
    let context_ref = env.new_global_ref(context).map_err(|e| e.to_string())?;
    ensure_loaded_from_thread(&jvm, &context_ref)
}

/// Ensures the engine is loaded. Safe to call from multiple threads concurrently.
///
/// - If already loaded, returns immediately.
/// - If another thread is loading, waits for it to finish.
/// - If no one is loading, this thread takes ownership of loading.
/// - If a previous load failed, retries.
///
/// Reports status via the target's `onStatusUpdate` JNI callback.
pub fn ensure_loaded_from_thread(
    jvm: &Arc<jni::JavaVM>,
    target_ref: &GlobalRef,
) -> Result<(), String> {
    let notify = |msg: &str| {
        if let Ok(mut env) = jvm.attach_current_thread() {
            notify_status(&mut env, target_ref.as_obj(), msg);
        }
    };

    // Fast path: already loaded
    if is_engine_loaded() {
        notify("Ready");
        return Ok(());
    }

    let (lock, cvar) = &*LOAD_STATE;
    let mut state = lock.lock().unwrap();

    // Re-check under lock
    if is_engine_loaded() {
        notify("Ready");
        return Ok(());
    }

    match &*state {
        LoadState::Loading => {
            // Another thread is loading — wait for it
            notify("Waiting for model...");
            while *state == LoadState::Loading {
                state = cvar.wait(state).unwrap();
            }
            drop(state);

            if is_engine_loaded() {
                notify("Ready");
                Ok(())
            } else {
                let msg = "Model failed to load".to_string();
                notify(&format!("Error: {}", msg));
                Err(msg)
            }
        }
        LoadState::Done => {
            notify("Ready");
            Ok(())
        }
        LoadState::Idle | LoadState::Failed(_) => {
            // We take ownership of loading (retry on previous failure)
            *state = LoadState::Loading;
            drop(state);

            let result = if let Ok(mut env) = jvm.attach_current_thread() {
                do_load(&mut env, target_ref.as_obj())
            } else {
                Err("Failed to attach JNI thread".to_string())
            };

            let mut state = lock.lock().unwrap();
            match &result {
                Ok(()) => *state = LoadState::Done,
                Err(msg) => *state = LoadState::Failed(msg.clone()),
            }
            cvar.notify_all();
            result
        }
    }
}

/// Inference thread count: the number of performance cores, capped at 4.
///
/// Phone SoCs are heterogeneous: fast cores paired with slow efficiency
/// cores. ggml synchronizes all threads after each operation, so a thread
/// on a slow core stalls the whole pool, and using every core is slower
/// than using only the fast ones. Cores are classified by their maximum
/// frequency from sysfs: within 70% of the fastest core counts as fast.
/// The count is capped at 4 because grabbing every fast core leaves none
/// for the rest of the system (audio pipeline, the app playing the sound),
/// and any preempted worker stalls the pool at the next op barrier; past
/// 4 threads the matmuls are memory-bound on phone-class SoCs anyway, and
/// more threads mainly build up heat. If sysfs is unreadable, falls back
/// to a conservative 4. The `model_threads` config file overrides the
/// heuristic.
fn performance_core_count() -> i32 {
    let mut freqs: Vec<u64> = Vec::new();
    for i in 0..64 {
        let path = format!("/sys/devices/system/cpu/cpu{i}/cpufreq/cpuinfo_max_freq");
        match std::fs::read_to_string(&path) {
            Ok(s) => match s.trim().parse::<u64>() {
                Ok(f) => freqs.push(f),
                Err(_) => break,
            },
            Err(_) => break,
        }
    }
    match freqs.iter().max() {
        Some(&max) if max > 0 => {
            let fast = freqs.iter().filter(|&&f| f * 10 >= max * 7).count();
            let threads = fast.clamp(1, 4);
            log::info!(
                "cpu clusters {:?} kHz -> {} performance cores -> {} threads",
                freqs,
                fast,
                threads
            );
            threads as i32
        }
        _ => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(4) as i32,
    }
}

/// CPU features this build's ggml kernels require (see GGML_CPU_ARM_ARCH in
/// the gradle build): dot-product and half-precision SIMD, present on arm64
/// cores since ~2018. Without this check, an older CPU would crash with an
/// illegal instruction mid-inference instead of showing an error.
#[cfg(target_arch = "aarch64")]
fn check_cpu_features() -> Result<(), String> {
    const HWCAP_ASIMHP: libc::c_ulong = 1 << 10; // FEAT_FP16 (asimdhp)
    const HWCAP_ASIMDDP: libc::c_ulong = 1 << 20; // FEAT_DotProd (asimddp)
    let hwcap = unsafe { libc::getauxval(libc::AT_HWCAP) };
    if hwcap & HWCAP_ASIMDDP == 0 || hwcap & HWCAP_ASIMHP == 0 {
        return Err(
            "this device's CPU is too old for this app version (needs arm64 \
             dotprod/fp16, available on phones from ~2018 on)"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(not(target_arch = "aarch64"))]
fn check_cpu_features() -> Result<(), String> {
    Ok(())
}

/// Reads a single-value config file (trimmed); `None` if absent or empty.
fn read_config(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Performs the model load: the bundled Nemotron GGUF extracted from APK
/// assets into filesDir.
fn do_load(env: &mut JNIEnv, context: &JObject) -> Result<(), String> {
    if let Err(msg) = check_cpu_features() {
        notify_status(env, context, &format!("Error: {}", msg));
        return Err(msg);
    }

    let files_dir = assets::files_dir(env, context).map_err(|e| {
        let msg = format!("Failed to resolve filesDir: {}", e);
        notify_status(env, context, &format!("Error: {}", msg));
        msg
    })?;
    let threads = read_config(&files_dir.join(MODEL_THREADS_FILE))
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(performance_core_count);

    notify_status(env, context, "Checking assets...");

    let path = assets::extract_builtin_model(env, context).map_err(|e| {
        let msg = format!("Asset error: {}", e);
        notify_status(env, context, &format!("Error: {}", msg));
        msg
    })?;

    notify_status(env, context, "Loading model...");

    match Engine::load(&path, threads) {
        Ok(engine) => {
            *GLOBAL_ENGINE.lock().unwrap() = Some(Arc::new(Mutex::new(engine)));
            notify_status(env, context, "Ready");
            Ok(())
        }
        Err(e) => {
            // Load failed — likely corrupt/incomplete extraction. Invalidate
            // it so the next attempt re-extracts from the APK.
            assets::invalidate_builtin_model(&path);
            let msg = format!("Model error: {}", e);
            notify_status(env, context, &format!("Error: {}", msg));
            Err(msg)
        }
    }
}
