use std::sync::Mutex;

use jni::objects::{JClass, JObject};
use jni::JNIEnv;
use once_cell::sync::Lazy;

use crate::voice_session::{self, VoiceSessionState};

static RECOG_STATE: Lazy<Mutex<Option<VoiceSessionState>>> = Lazy::new(|| Mutex::new(None));

#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_RecognizeActivity_initNative(
    env: JNIEnv,
    _class: JClass,
    activity: JObject,
) {
    match voice_session::init_session(env, activity) {
        Ok(state) => *RECOG_STATE.lock().unwrap() = Some(state),
        Err(e) => log::error!("RecognizeActivity session init failed: {}", e),
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_RecognizeActivity_cleanupNative(
    env: JNIEnv,
    _class: JClass,
) {
    let mut guard = RECOG_STATE.lock().unwrap();
    // Same as the IME cleanup: cancel any live session before dropping the
    // state, or its consumer survives as an orphan holding the compute
    // lease (and the mic) with no way to stop it.
    if let Some(state) = guard.as_mut() {
        voice_session::cancel_recording(env, state);
    }
    *guard = None;
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_RecognizeActivity_startRecording(
    env: JNIEnv,
    _class: JClass,
    auto_stop: jni::sys::jboolean,
) {
    let mut guard = RECOG_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
        voice_session::start_recording(env, state, auto_stop != 0);
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_RecognizeActivity_stopRecording(
    env: JNIEnv,
    _class: JClass,
) {
    let mut guard = RECOG_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
        voice_session::stop_recording(env, state);
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_jamesnicholls_nemotronvoice_RecognizeActivity_cancelRecording(
    env: JNIEnv,
    _class: JClass,
) {
    let mut guard = RECOG_STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
        crate::voice_session::cancel_recording(env, state);
    }
}
