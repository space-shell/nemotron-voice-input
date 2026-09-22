use jni::objects::{JClass, JObject};
use jni::JNIEnv;
use once_cell::sync::Lazy;
use std::sync::Mutex;

use crate::voice_session::{self, VoiceSessionState};

static IME_STATE: Lazy<Mutex<Option<VoiceSessionState>>> = Lazy::new(|| Mutex::new(None));

#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_RustInputMethodService_initNative(
    env: JNIEnv,
    _class: JClass,
    service: JObject,
) {
    let state = voice_session::init_session(env, service);
    *IME_STATE.lock().unwrap() = Some(state);
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_RustInputMethodService_cleanupNative(
    env: JNIEnv,
    _class: JClass,
) {
    let mut guard = IME_STATE.lock().unwrap();
    // A live session must be cancelled, not just dropped: the consumer
    // thread owns its Arcs and survives the state swap. Samsung's freezer
    // destroys and re-creates the IME service inside a live process whenever
    // the keyboard is deselected; without this, a session that was recording
    // at destroy time became an orphan that kept the mic on and held the
    // model's compute lease forever — every later recording failed Busy
    // until the process died.
    if let Some(state) = guard.as_mut() {
        voice_session::cancel_recording(env, state);
    }
    *guard = None;
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_RustInputMethodService_startRecording(
    env: JNIEnv,
    _class: JClass,
) {
    let mut guard = IME_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
        // The IME keyboard is manual tap-to-stop; no silence auto-stop.
        voice_session::start_recording(env, state, false);
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_RustInputMethodService_stopRecording(
    env: JNIEnv,
    _class: JClass,
) {
    let mut guard = IME_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
        voice_session::stop_recording(env, state);
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_RustInputMethodService_cancelRecording(
    env: JNIEnv,
    _class: JClass,
) {
    let mut guard = IME_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
        voice_session::cancel_recording(env, state);
    }
}
