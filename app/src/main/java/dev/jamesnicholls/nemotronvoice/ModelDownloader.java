package dev.jamesnicholls.nemotronvoice;

import android.content.Context;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.security.MessageDigest;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * One-time download of the speech model. The APK ships without the ~700 MB
 * GGUF; this fetches it on first launch, checksum-verifies the bytes while
 * streaming (nothing unverified is ever exposed to the engine), and only
 * then writes the completion marker that {@code assets.rs} requires.
 */
public final class ModelDownloader {
    // Pinned upstream artifact — same URL and sha256 the old build-time
    // gradle task used, so the bytes are identical to the previously
    // bundled model.
    private static final String MODEL_URL =
            "https://huggingface.co/handy-computer/nemotron-speech-streaming-en-0.6b-gguf"
                    + "/resolve/main/nemotron-speech-streaming-en-0.6b-Q8_0.gguf?download=true";
    private static final String MODEL_SHA256 =
            "90d8c89714cd31efc88be62a40c6b2bea57e0cc2063af1ffe2c28f1a228ca110";
    private static final String MODEL_FILE = "nemotron-speech-streaming-en-0.6b-Q8_0.gguf";

    private static final String MODEL_DIR = "builtin-model";
    // Marker names must match src/assets.rs.
    private static final String MARKER = ".download_complete";
    private static final String LEGACY_MARKER = ".extraction_complete";

    /** Same layout the bundled-asset era wrote: upgrades keep their model. */
    public static File modelFile(File filesDir) {
        return new File(new File(filesDir, MODEL_DIR), MODEL_FILE);
    }

    /** True once a complete model exists — either a verified download or a
     *  leftover extraction from the bundled-asset app versions. */
    public static boolean isDownloaded(File filesDir) {
        File dir = new File(filesDir, MODEL_DIR);
        return (new File(dir, MARKER).exists() || new File(dir, LEGACY_MARKER).exists())
                && modelFile(filesDir).isFile();
    }

    public interface Listener {
        /** Download progress; {@code percent} is 0-100 (unknown size: -1). */
        void onProgress(int percent);
        void onDone(boolean ok, String error);
    }

    private final ExecutorService executor = Executors.newSingleThreadExecutor();

    /** Runs the download on a background thread; callbacks are invoked there
     *  — marshal to the main thread at the call site. */
    public void downloadAsync(Context context, Listener listener) {
        File dir = new File(context.getFilesDir(), MODEL_DIR);
        File target = new File(dir, MODEL_FILE);
        File partial = new File(dir, MODEL_FILE + ".part");
        executor.execute(() -> {
            try {
                dir.mkdirs();
                if (target.isFile()) {
                    // A previous download finished the bytes but the marker
                    // was removed (engine flagged corruption, or an upgrade
                    // wrote only part of the state): re-verify instead of
                    // re-downloading ~700 MB.
                    if (verify(target)) {
                        finish(target, listener);
                        return;
                    }
                    target.delete();
                }
                doDownload(partial, target, listener);
            } catch (IOException | RuntimeException e) {
                partial.delete();
                listener.onDone(false, e.getMessage() == null ? e.getClass().getSimpleName() : e.getMessage());
            }
        });
    }

    private void doDownload(File partial, File target, Listener listener) throws IOException {
        HttpURLConnection conn = (HttpURLConnection) new URL(MODEL_URL).openConnection();
        try {
            conn.setConnectTimeout(15000);
            conn.setReadTimeout(30000);
            int status = conn.getResponseCode();
            if (status != HttpURLConnection.HTTP_OK) {
                throw new IOException("HTTP " + status);
            }
            long total = conn.getContentLengthLong();
            MessageDigest digest;
            try {
                digest = MessageDigest.getInstance("SHA-256");
            } catch (java.security.NoSuchAlgorithmException e) {
                throw new IllegalStateException(e);
            }

            try (InputStream in = conn.getInputStream();
                 OutputStream out = new FileOutputStream(partial)) {
                byte[] buf = new byte[64 * 1024];
                long done = 0;
                int read;
                int lastPercent = -1;
                while ((read = in.read(buf)) != -1) {
                    out.write(buf, 0, read);
                    digest.update(buf, 0, read);
                    done += read;
                    if (total > 0) {
                        int percent = (int) (done * 100 / total);
                        if (percent != lastPercent) {
                            lastPercent = percent;
                            listener.onProgress(percent);
                        }
                    }
                }
            }
            String hash = hex(digest.digest());
            if (!hash.equalsIgnoreCase(MODEL_SHA256)) {
                partial.delete();
                throw new IOException("checksum mismatch");
            }
            if (!partial.renameTo(target)) {
                throw new IOException("could not move downloaded file into place");
            }
            finish(target, listener);
        } finally {
            conn.disconnect();
        }
    }

    /** Verifies an existing (unmarked) model file against the pinned hash. */
    private boolean verify(File file) {
        try (InputStream in = new java.io.FileInputStream(file)) {
            MessageDigest digest = MessageDigest.getInstance("SHA-256");
            byte[] buf = new byte[64 * 1024];
            int read;
            while ((read = in.read(buf)) != -1) {
                digest.update(buf, 0, read);
            }
            if (hex(digest.digest()).equalsIgnoreCase(MODEL_SHA256)) {
                return true;
            }
        } catch (IOException | java.security.NoSuchAlgorithmException e) {
            return false;
        }
        return false;
    }

    private void finish(File target, Listener listener) throws IOException {
        // Marker last: assets.rs treats its absence as "no model".
        if (!new File(target.getParentFile(), MARKER).createNewFile()) {
            throw new IOException("could not write completion marker");
        }
        listener.onDone(true, null);
    }

    private static String hex(byte[] bytes) {
        StringBuilder sb = new StringBuilder(bytes.length * 2);
        for (byte b : bytes) {
            sb.append(Character.forDigit((b >> 4) & 0xf, 16))
              .append(Character.forDigit(b & 0xf, 16));
        }
        return sb.toString();
    }
}
