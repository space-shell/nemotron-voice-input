//! Location and validation of the on-device speech model.
//!
//! The GGUF is no longer bundled in the APK: the app downloads it once on
//! first launch (`ModelDownloader`, main process) into
//! `filesDir/builtin-model/` — the same layout the bundled-asset era
//! wrote, so upgrades keep an already-extracted model. This module only
//! resolves and validates the file; native code never touches the network.

use jni::objects::JObject;
use jni::JNIEnv;
use std::path::{Path, PathBuf};

/// filesDir subdirectory holding the model GGUF.
const BUILTIN_MODEL_DIR: &str = "builtin-model";
/// Marker file present once a complete, checksum-verified download exists.
/// Without it the directory is treated as incomplete. Removed by the engine
/// when a load fails on (likely corrupt) files, which makes the app offer
/// the download again.
const DOWNLOAD_COMPLETE_MARKER: &str = ".download_complete";
/// Marker file written by the bundled-asset era; treated as equivalent to
/// a completed download (those bytes were verified at build time).
const LEGACY_EXTRACTION_MARKER: &str = ".extraction_complete";
/// Model directory of the pre-GGUF (ONNX) app versions; deleted on sight so
/// upgrades don't leave ~670 MB of dead files behind.
const LEGACY_MODEL_DIR: &str = "parakeet-tdt-0.6b-v3-int8";

/// Resolves the app's `filesDir` via the given Context.
pub fn files_dir(env: &mut JNIEnv, context: &JObject) -> anyhow::Result<PathBuf> {
    let files_dir_obj = env
        .call_method(context, "getFilesDir", "()Ljava/io/File;", &[])?
        .l()?;
    let path_str_obj = env
        .call_method(
            &files_dir_obj,
            "getAbsolutePath",
            "()Ljava/lang/String;",
            &[],
        )?
        .l()?;
    let path_string: String = env.get_string(&path_str_obj.into())?.into();
    Ok(PathBuf::from(path_string))
}

/// Returns the path of the model GGUF, or an error directing the user to
/// download it from the app. Only the completion marker is checked here:
/// the downloader verified the bytes when it wrote them, and the engine
/// detects any later corruption and removes the marker.
pub fn ensure_model(env: &mut JNIEnv, context: &JObject) -> anyhow::Result<PathBuf> {
    let base_path = files_dir(env, context)?;

    let legacy_dir = base_path.join(LEGACY_MODEL_DIR);
    if legacy_dir.exists() {
        log::info!("Removing legacy ONNX model directory");
        let _ = std::fs::remove_dir_all(&legacy_dir);
    }

    let model_dir = base_path.join(BUILTIN_MODEL_DIR);
    let complete = model_dir.join(DOWNLOAD_COMPLETE_MARKER).exists()
        || model_dir.join(LEGACY_EXTRACTION_MARKER).exists();
    if !complete {
        anyhow::bail!("speech model not downloaded yet — open the app to download it");
    }

    find_gguf(&model_dir)
}

/// Removes the completion marker so the app re-offers the download —
/// called when a load fails on (likely corrupt) downloaded files.
pub fn invalidate_model(model_path: &Path) {
    if let Some(dir) = model_path.parent() {
        for marker in [DOWNLOAD_COMPLETE_MARKER, LEGACY_EXTRACTION_MARKER] {
            let marker = dir.join(marker);
            if marker.exists() {
                log::warn!("Model load failed, removing completion marker");
                let _ = std::fs::remove_file(&marker);
            }
        }
    }
}

/// Returns the single `.gguf` file in `dir`.
fn find_gguf(dir: &Path) -> anyhow::Result<PathBuf> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("gguf")) {
            return Ok(path);
        }
    }
    anyhow::bail!("no GGUF file found in {}", dir.display())
}
