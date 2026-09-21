package dev.jamesnicholls.nemotronvoice;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.speech.RecognizerIntent;
import android.util.Log;
import android.view.WindowManager;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;
import android.content.pm.PackageManager;

import androidx.appcompat.app.AppCompatActivity;

import java.util.ArrayList;

public class RecognizeActivity extends AppCompatActivity {

    private static final String TAG = "OfflineVoiceInput";

    static {
        try {
            System.loadLibrary("c++_shared");
            System.loadLibrary("nemotron_voice_input");
        } catch (UnsatisfiedLinkError e) {
            Log.e(TAG, "Failed to load native libraries", e);
        }
    }

    private TextView status;
    private boolean isRecording = false;
    // True between onCreate and the engine's first "Ready": capture must not
    // start while the model loads (cold popup). The loader thread spawned by
    // initNative drives the transition via onStatusUpdate.
    private boolean pendingStart = false;
    private MicLevelView micLevel;
    private final AudioFocusPauser audioPauser = new AudioFocusPauser();
    private boolean pauseAudioActive = false;
    // Set between stop and the final transcript; a watchdog turns an engine
    // wedge into a visible error instead of infinite "Processing" (#16).
    private boolean awaitingResult = false;
    private static final long STALL_TIMEOUT_MS = 30_000; // ms
    private final Runnable stallRunnable = () -> {
        if (!awaitingResult || isFinishing()) return;
        awaitingResult = false;
        Log.e(TAG, "no final transcript after " + STALL_TIMEOUT_MS + "ms; reporting stall");
        status.setText("Error: transcription stalled — close and retry");
        // Abandon the stream so session flags stay consistent. The wedged
        // native consumer stays wedged (recovery is an app restart, #16),
        // but the popup stops pretending to work.
        try { cancelRecording(); } catch (Throwable t) { /* ignore */ }
    };

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        setContentView(R.layout.recognize_activity);

        // Keep the screen awake for the lifetime of this recording screen so it
        // never sleeps mid-capture and cuts the recording short.
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);

        micLevel = findViewById(R.id.mic_level);
        status = findViewById(R.id.txt_status);

        findViewById(R.id.btn_close).setOnClickListener(v -> {
            // discard current recording
            pendingStart = false;
            if (isRecording) {
                isRecording = false;
                cancelRecording();   // new native method
            }
            if (pauseAudioActive) {
                audioPauser.abandon(this);
                pauseAudioActive = false;
            }
            setResult(Activity.RESULT_CANCELED);
            finish();
        });

        // Tap anywhere (or on mic) to stop
        findViewById(R.id.root).setOnClickListener(v -> finishRecording());

        // Permission check
        if (checkSelfPermission(android.Manifest.permission.RECORD_AUDIO)
                != PackageManager.PERMISSION_GRANTED) {
            status.setText("Microphone permission required.\nGrant it in the main app.");
            return;
        }

        initNative(this);
        // Defer capture until the engine reports "Ready". Starting
        // immediately on a cold process shows a live mic meter and
        // "Listening" while the consumer thread is actually still waiting
        // for the model — and if that load wedges, endpointing flips the
        // popup to "Processing" forever (#16).
        pendingStart = true;
        status.setText("Loading model…");
    }

    /** Start capture once the engine is ready (deferred onCreate path or
     *  the "Ready" status callback). */
    private void beginListening() {
        isRecording = true;
        status.setText("Listening... (Tap to stop)");
        if (isPauseAudioEnabled()) {
            audioPauser.request(this);
            pauseAudioActive = true;
        }
        startRecording(isAutoStopEnabled());
    }

    /** Stop capture and transcribe — used by both tap-to-stop and auto-stop. */
    private void finishRecording() {
        if (!isRecording) return;
        isRecording = false;
        status.setText("Processing...");
        stopRecording();
        // Watchdog: the final transcript normally lands in well under a
        // second; if nothing arrives the engine wedged (#16) — say so
        // instead of hanging at "Processing" forever.
        awaitingResult = true;
        status.postDelayed(stallRunnable, STALL_TIMEOUT_MS);
        if (pauseAudioActive) {
            audioPauser.abandon(this);
            pauseAudioActive = false;
        }
    }

    // Called from Rust (monitor thread) when trailing silence is detected.
    public void onAutoStop() {
        runOnUiThread(this::finishRecording);
    }

    @Override
    protected void onStop() {
        super.onStop();
        // The popup is no longer visible (user switched apps or went home).
        // It runs in its own task (singleTask), so it would otherwise keep
        // recording invisibly in the background and never reappear. Discard
        // and close so the next mic tap starts fresh. (Background recording
        // is a keyboard-only feature; a popup must not record unseen.)
        if ((isRecording || pendingStart) && !isFinishing()) {
            isRecording = false;
            pendingStart = false;
            try { cancelRecording(); } catch (Throwable t) { /* ignore */ }
            if (pauseAudioActive) {
                audioPauser.abandon(this);
                pauseAudioActive = false;
            }
            setResult(Activity.RESULT_CANCELED);
            finish();
        }
    }

    @Override
    protected void onDestroy() {
        super.onDestroy();
        if (pauseAudioActive) {
            audioPauser.abandon(this);
            pauseAudioActive = false;
        }
        try { cleanupNative(); } catch (Throwable t) { /* ignore */ }
    }

    // Called from Rust
    public void onStatusUpdate(String s) {
        runOnUiThread(() -> {
            if ("Ready".equals(s) && pendingStart) {
                pendingStart = false;
                if (!isFinishing()) beginListening();
                return;
            }
            final String shown;
            if ("Ready".equals(s)) {
                // The model-ready status can arrive after recording started.
                shown = isRecording ? "Listening... (Tap to stop)" : "Ready";
            } else if ("Listening...".equals(s)) {
                shown = "Listening... (Tap to stop)";
            } else {
                if (s != null && s.startsWith("Error")) pendingStart = false;
                shown = s;
            }
            status.setText(shown);
        });
    }

    // Called from Rust with 0..1
    public void onAudioLevel(float level) {
        runOnUiThread(() -> micLevel.setLevel(level));
    }

    // Called from Rust – keep same method name as IME for code reuse
    public void onTextTranscribed(String text) {
        runOnUiThread(() -> {
            awaitingResult = false;
            status.removeCallbacks(stallRunnable);
            if (text == null || text.trim().isEmpty()) {
                // Nothing was recognized (e.g. auto-stop after silence only).
                setResult(Activity.RESULT_CANCELED);
                finish();
                return;
            }

            ArrayList<String> results = new ArrayList<>();
            results.add(text);

            Intent data = new Intent();
            data.putStringArrayListExtra(RecognizerIntent.EXTRA_RESULTS, results);

            setResult(Activity.RESULT_OK, data);
            finish();
        });
    }

    // Called from Rust during streaming — show the live hypothesis in the
    // panel so the user sees text appear while speaking.
    public void onPartialText(String committed, String tentative) {
        runOnUiThread(() -> {
            if (!isRecording) return;
            String live = (committed == null ? "" : committed) + (tentative == null ? "" : tentative);
            if (!live.isEmpty()) {
                status.setText(live);
            }
        });
    }

    private boolean isPauseAudioEnabled() {
        return new java.io.File(getFilesDir(), "pause_audio").exists();
    }

    /** Opt-in via the "Auto-stop after silence" setting (default off). */
    private boolean isAutoStopEnabled() {
        return new java.io.File(getFilesDir(), "auto_stop").exists();
    }

    // Native methods
    private native void initNative(RecognizeActivity activity);
    private native void cleanupNative();
    private native void startRecording(boolean autoStop);
    private native void stopRecording();
    private native void cancelRecording();
}
