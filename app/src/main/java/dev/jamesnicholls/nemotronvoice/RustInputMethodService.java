package dev.jamesnicholls.nemotronvoice;

import android.inputmethodservice.InputMethodService;
import android.view.LayoutInflater;
import android.view.View;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;
import android.widget.ProgressBar;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import android.content.Context;
import android.content.pm.PackageManager;
import android.view.MotionEvent;
import android.view.inputmethod.EditorInfo;
import android.content.res.ColorStateList;
import android.view.ContextThemeWrapper;
import java.io.File;

import com.google.android.material.color.DynamicColors;
import com.google.android.material.color.MaterialColors;

public class RustInputMethodService extends InputMethodService {
    
    private static final String TAG = "OfflineVoiceInput";

    static {
        try {
            System.loadLibrary("c++_shared");
            System.loadLibrary("nemotron_voice_input");
        } catch (UnsatisfiedLinkError e) {
            Log.e(TAG, "Failed to load native libraries", e);
        }
    }

    private TextView statusView;
    private TextView hintView;
    private View recordContainer;
    private android.widget.ImageView micIcon;
    private ProgressBar progressBar;
    private View backspaceButton;
    private View spaceButton;
    private View enterButton;
    private View switchKeyboardButton;
    private View inputView;
    private MicLevelView micLevelView;
    private View recordCircle;
    // Night flag the current input view was inflated with, so it can be rebuilt
    // if the theme preference changes while this process stays alive.
    private boolean viewIsNight = false;
    // Switch-back flag the current input view was inflated with (minimal key
    // row); rebuilt like viewIsNight when the setting changes in-app.
    private boolean viewIsMinimal = false;
    private Handler mainHandler;
    private boolean isRecording = false;
    // Mirrored from the Rust engine's status callbacks: true once the model
    // has finished loading ("Ready"). Recording before this is what made
    // cold starts lose audio (the consumer waits for the load while the
    // bounded audio channel overflows).
    private boolean modelReady = false;
    // Auto-record requested while the model was still loading; fires once
    // "Ready" arrives (and the window is still shown).
    private boolean pendingAutoStart = false;
    // Return key pressed mid-recording: stop first, then send (perform the
    // editor's enter action) once the final text has been committed — the
    // finalize is async, so sending immediately would fire before the last
    // words land in the field.
    private boolean pendingSend = false;
    private boolean pendingSwitchBack = false;
    private String lastStatus = "Initializing...";
    // Key repeat settings
    private static final long REPEAT_INITIAL_DELAY = 400; // ms before repeat starts
    private static final long REPEAT_INTERVAL = 50; // ms between repeats
    private Runnable backspaceRepeatRunnable;
    private Runnable spaceRepeatRunnable;
    private final AudioFocusPauser audioPauser = new AudioFocusPauser();
    private boolean pauseAudioActive = false;
    // Whether an editor is currently focused/started for input. Tracked via
    // onStartInput/onFinishInput because getCurrentInputConnection() returns a
    // non-null no-op connection when nothing is focused, so commitText would be
    // silently dropped.
    private boolean inputActive = false;
    // Whether the keyboard window is currently on screen. Some frameworks
    // (notably OEM builds) call onWindowShown again for events that don't
    // follow an onWindowHidden, e.g. tapping the text area to move the
    // cursor while the keyboard stays visible. Auto-record must only fire on
    // a genuine hidden -> shown transition, or a cursor tap starts a
    // recording the user never asked for.
    private boolean windowVisible = false;
    // Transcribed text waiting to be committed because no editor was focused
    // when transcription finished. This happens on long transcribes where the
    // target field (e.g. a web field in Firefox/Gemini) drops focus while we
    // process audio. Flushed from onStartInputView once a field is focused
    // again so the text is never lost.
    private String pendingCommitText = null;
    // Prefix of the current streaming utterance already committed to the
    // field via commitText. The engine's committed text only grows, so the
    // delta (committed - committedBase) is what still needs committing. The
    // FINAL full text is a separate matter: transcribe.cpp keeps committed
    // append-only but lets the authoritative full text disagree with it (a
    // late revision — see onTextTranscribed for the repair).
    private String committedBase = "";
    // Tail last pushed as the field's composing region (onPartialText's
    // `tentative`). On focus loss the editor finalizes a composing region
    // into plain text — it must not lose visible text — so after a
    // mid-dictation focus drop the field holds committedBase + lastTentative,
    // and the deferred remainder in onTextTranscribed is computed against
    // exactly that.
    private String lastTentative = "";
    // Floating-keyboard state (drag handling).
    private boolean floatingMode = false;
    private int floatingWidthPx = 0;

    @Override
    public void onCreate() {
        super.onCreate();
        mainHandler = new Handler(Looper.getMainLooper());
        Log.d(TAG, "Service onCreate");
        try {
            initNative(this);
        } catch (Throwable t) {
            // Native may be unavailable (e.g. wrong-ABI emulator); don't crash the IME.
            Log.e(TAG, "Error in initNative", t);
        }
    }

    @Override
    public View onCreateInputView() {
        Log.d(TAG, "onCreateInputView");
        try {
            // The IME is a non-AppCompat Service in a separate process, so
            // AppCompat's delegate can't theme it. Build a context that is
            // night-aware (per the saved preference), wears the Material 3 theme,
            // and picks up Material You dynamic color — matching the app.
            Context night = ThemePrefs.wrapForNight(this, ThemePrefs.getMode(this));
            viewIsNight = ThemePrefs.isNight(night);
            Context themed = DynamicColors.wrapContextIfAvailable(
                    new ContextThemeWrapper(night, R.style.AppTheme));
            View view = LayoutInflater.from(themed).inflate(R.layout.ime_layout, null);
            inputView = view;

            // Floating keyboard mode: the IME window becomes a movable,
            // wrap-content panel instead of a docked full-width slab. The
            // window keeps input focus (the InputConnection is unaffected);
            // a drag handle repositions it and the position is remembered.
            floatingMode = isFloatingKeyboardEnabled();
            View dragHandle = view.findViewById(R.id.ime_drag_handle);
            if (floatingMode) {
                dragHandle.setVisibility(View.VISIBLE);
                applyFloatingWindow();
                attachDragHandler(dragHandle);
            } else {
                // Reset any layout params a previous floating session left on
                // the window (gravity/x/y survive view rebuilds).
                applyDockedWindow();
            }

            // Handle window insets for avoiding navigation bar overlap (only
            // meaningful when docked — a floating panel isn't edge-attached).
            if (!floatingMode) {
                view.setOnApplyWindowInsetsListener((v, insets) -> {
                    int paddingBottom = insets.getSystemWindowInsetBottom();
                    int originalPaddingBottom = v.getPaddingTop();
                    v.setPadding(v.getPaddingLeft(), v.getPaddingTop(), v.getPaddingRight(), originalPaddingBottom + paddingBottom);
                    return insets;
                });
            }

            statusView = view.findViewById(R.id.ime_status_text);
            progressBar = view.findViewById(R.id.ime_progress);
            recordContainer = view.findViewById(R.id.ime_record_container);
            micIcon = view.findViewById(R.id.ime_mic_icon);
            micLevelView = view.findViewById(R.id.ime_mic_level);
            recordCircle = view.findViewById(R.id.ime_record_circle);
            hintView = view.findViewById(R.id.ime_hint);
            backspaceButton = view.findViewById(R.id.ime_backspace);
            spaceButton = view.findViewById(R.id.ime_space);
            enterButton = view.findViewById(R.id.ime_enter);
            switchKeyboardButton = view.findViewById(R.id.ime_switch_keyboard);

            // Minimal mode (auto return to previous keyboard, the default):
            // the editing keys are never reached — the keyboard closes as
            // soon as dictation ends — so hide backspace and space and show
            // only the switch key (left) and the return key (right), which
            // stops the dictation and sends the text (e.g. a WhatsApp
            // message).
            viewIsMinimal = isSwitchBackEnabled();
            if (viewIsMinimal) {
                backspaceButton.setVisibility(View.GONE);
                spaceButton.setVisibility(View.GONE);
                view.findViewById(R.id.ime_row_spacer).setVisibility(View.VISIBLE);
                hintView.setVisibility(View.GONE);
            }

            switchKeyboardButton.setOnClickListener(v -> {
                if (isRecording) {
                    pendingSwitchBack = true;
                    stopRecording();
                    updateRecordButtonUI(false);
                } else {
                    switchToPreviousInputMethod();
                }
            });

            // Key repeat runnable for backspace
            backspaceRepeatRunnable = new Runnable() {
                @Override
                public void run() {
                    InputConnection ic = getCurrentInputConnection();
                    if (ic != null) {
                        ic.sendKeyEvent(new android.view.KeyEvent(android.view.KeyEvent.ACTION_DOWN, android.view.KeyEvent.KEYCODE_DEL));
                        ic.sendKeyEvent(new android.view.KeyEvent(android.view.KeyEvent.ACTION_UP, android.view.KeyEvent.KEYCODE_DEL));
                    }
                    mainHandler.postDelayed(this, REPEAT_INTERVAL);
                }
            };

            // Key repeat runnable for space
            spaceRepeatRunnable = new Runnable() {
                @Override
                public void run() {
                    InputConnection ic = getCurrentInputConnection();
                    if (ic != null) {
                        ic.commitText(" ", 1);
                    }
                    mainHandler.postDelayed(this, REPEAT_INTERVAL);
                }
            };

            backspaceButton.setOnTouchListener((v, event) -> {
                switch (event.getAction()) {
                    case MotionEvent.ACTION_DOWN:
                        InputConnection ic = getCurrentInputConnection();
                        if (ic != null) {
                            ic.sendKeyEvent(new android.view.KeyEvent(android.view.KeyEvent.ACTION_DOWN, android.view.KeyEvent.KEYCODE_DEL));
                            ic.sendKeyEvent(new android.view.KeyEvent(android.view.KeyEvent.ACTION_UP, android.view.KeyEvent.KEYCODE_DEL));
                        }
                        mainHandler.postDelayed(backspaceRepeatRunnable, REPEAT_INITIAL_DELAY);
                        return true;
                    case MotionEvent.ACTION_UP:
                    case MotionEvent.ACTION_CANCEL:
                        mainHandler.removeCallbacks(backspaceRepeatRunnable);
                        return true;
                }
                return false;
            });

            spaceButton.setOnTouchListener((v, event) -> {
                switch (event.getAction()) {
                    case MotionEvent.ACTION_DOWN:
                        InputConnection ic = getCurrentInputConnection();
                        if (ic != null) {
                            ic.commitText(" ", 1);
                        }
                        mainHandler.postDelayed(spaceRepeatRunnable, REPEAT_INITIAL_DELAY);
                        return true;
                    case MotionEvent.ACTION_UP:
                    case MotionEvent.ACTION_CANCEL:
                        mainHandler.removeCallbacks(spaceRepeatRunnable);
                        return true;
                }
                return false;
            });

            enterButton.setOnClickListener(v -> onReturnKey());

            recordContainer.setOnClickListener(v -> {
                if (!recordContainer.isEnabled()) return;

                // Check microphone permission
                if (checkSelfPermission(android.Manifest.permission.RECORD_AUDIO)
                        != PackageManager.PERMISSION_GRANTED) {
                    if (statusView != null) statusView.setText("No mic permission - grant in app");
                    if (hintView != null) hintView.setText("Open the app to grant permission");
                    return;
                }

                if (isRecording) {
                    stopRecording();
                    if (pauseAudioActive) {
                        audioPauser.abandon(this);
                        pauseAudioActive = false;
                    }
                    updateRecordButtonUI(false);
                } else {
                    if (!modelReady) {
                        // Still loading — ignore the tap (the button should
                        // be disabled, this guards against a stale tap in
                        // flight). After a load *error* the tap is a retry:
                        // fall through and let the consumer re-load.
                        if (!lastStatus.startsWith("Error")) {
                            updateUiState();
                            return;
                        }
                    }
                    beginRecording();
                }
            });

            // Hold the record button to discard the recording (#66): unlike
            // stop, cancel abandons the stream without committing text.
            recordContainer.setOnLongClickListener(v -> {
                if (!recordContainer.isEnabled() || !isRecording) return false;
                try {
                    cancelRecording();
                } catch (Throwable t) {
                    Log.w(TAG, "cancelRecording failed", t);
                }
                if (pauseAudioActive) {
                    audioPauser.abandon(this);
                    pauseAudioActive = false;
                }
                committedBase = "";
                lastTentative = "";
                pendingSend = false;
                InputConnection ic = getCurrentInputConnection();
                if (ic != null) ic.finishComposingText();
                updateRecordButtonUI(false);
                if (statusView != null) statusView.setText("Canceled");
                // A canceled dictation also ends the session: with auto
                // switch-back, hand the keyboard back instead of stranding
                // the user on a text-less voice keyboard.
                if (pendingSwitchBack || isSwitchBackEnabled()) {
                    pendingSwitchBack = false;
                    switchToPreviousInputMethod();
                }
                return true;
            });

            tintRecordButton(false);
            updateUiState();
            return view;
        } catch (Exception e) {
            Log.e(TAG, "Error in onCreateInputView", e);
            TextView errorView = new TextView(this);
            errorView.setText("Error loading keyboard: " + e.getMessage());
            return errorView;
        }
    }

    @Override
    public void onWindowShown() {
        super.onWindowShown();
        // The framework resets the IME window to MATCH_PARENT x WRAP_CONTENT
        // on show transitions (InputMethodService#onConfigureWindow); the
        // floating params must be re-applied or the panel renders wrong.
        if (floatingMode) applyFloatingWindow();
        boolean wasVisible = windowVisible;
        windowVisible = true;
        if (isRecording) {
            // A background recording is still running (record-in-background
            // setting): restore the recording UI.
            updateRecordButtonUI(true);
            return;
        }
        if (wasVisible) {
            // Not a real hidden -> shown transition (e.g. a cursor tap in the
            // text area); never auto-start a recording from here.
            return;
        }
        if (new File(getFilesDir(), "auto_record").exists()) {
            if (checkSelfPermission(android.Manifest.permission.RECORD_AUDIO)
                    == PackageManager.PERMISSION_GRANTED) {
                if (!modelReady) {
                    // Model still loading: defer the auto-start until the
                    // engine reports Ready, or the buffered audio overflows
                    // and the recording is discarded.
                    pendingAutoStart = true;
                    updateUiState();
                    return;
                }
                beginRecording();
            }
        }
    }

    @Override
    public void onWindowHidden() {
        super.onWindowHidden();
        windowVisible = false;
        pendingAutoStart = false;
        if (isRecording) {
            if (isStopOnHideEnabled()) {
                // Opt-in behavior: discard the recording when the keyboard hides.
                try {
                    cancelRecording();
                } catch (Throwable t) {
                    Log.w(TAG, "cancelRecording failed, falling back to stopRecording", t);
                    try { stopRecording(); } catch (Throwable ignored) { }
                }
                updateRecordButtonUI(false);
            } else {
                // Default: keep recording in the background. The transcription
                // is committed on return (or held in pendingCommitText).
                return;
            }
        }
        if (pauseAudioActive) {
            audioPauser.abandon(this);
            pauseAudioActive = false;
        }
    }

    @Override
    public void onStartInput(EditorInfo attribute, boolean restarting) {
        super.onStartInput(attribute, restarting);
        inputActive = true;
    }

    @Override
    public void onStartInputView(EditorInfo info, boolean restarting) {
        super.onStartInputView(info, restarting);
        inputActive = true;
        // Rebuild the keyboard if the theme preference changed while this
        // (long-lived) IME process stayed alive, so it matches the app setting.
        if (inputView != null
                && ThemePrefs.isNight(ThemePrefs.wrapForNight(this, ThemePrefs.getMode(this))) != viewIsNight) {
            setInputView(onCreateInputView());
            // The insets pass has already run for the window, so the rebuilt
            // view never receives it — request one explicitly or the nav-bar
            // bottom padding (and with it the bottom key row) is lost
            // (upstream #99 / PR #100).
            if (inputView != null) inputView.requestApplyInsets();
        }
        // The floating / minimal-keyboard markers can be toggled in the main
        // app while this IME process stays alive; they are otherwise only
        // read when the input view is (re)created. Rebuild so the toggle
        // takes effect.
        if (isFloatingKeyboardEnabled() != floatingMode
                || isSwitchBackEnabled() != viewIsMinimal) {
            setInputView(onCreateInputView());
            if (inputView != null) inputView.requestApplyInsets();
        }
        // A field is focused and the input connection is live again — commit any
        // text that finished transcribing while nothing was focused.
        flushPendingText();
    }

    @Override
    public void onFinishInput() {
        super.onFinishInput();
        inputActive = false;
    }

    /** Shared by the auto-record path and the deferred auto-start: pauses
     *  competing audio (if enabled), opens the mic, and flips the UI into
     *  the recording state. */
    private void beginRecording() {
        pendingSend = false;
        if (isPauseAudioEnabled()) {
            audioPauser.request(this);
            pauseAudioActive = true;
        }
        startRecording();
        updateRecordButtonUI(true);
    }

    /** The return key: mid-recording it stops the dictation and sends the
     *  text once it has landed (the finalize is async); otherwise it acts
     *  as a plain enter — e.g. IME_ACTION_SEND in WhatsApp sends the
     *  message. */
    private void onReturnKey() {
        if (isRecording) {
            pendingSend = true;
            stopRecording();
            if (pauseAudioActive) {
                audioPauser.abandon(this);
                pauseAudioActive = false;
            }
            updateRecordButtonUI(false);
        } else {
            performEnterAction();
        }
    }

    /** Performs the editor's enter action (Go/Search/Send/Next) when the
     *  field asks for one, else inserts a newline via key events. */
    private void performEnterAction() {
        InputConnection ic = getCurrentInputConnection();
        if (ic != null) {
            android.view.inputmethod.EditorInfo editorInfo = getCurrentInputEditorInfo();
            if (editorInfo == null) {
                ic.sendKeyEvent(new android.view.KeyEvent(android.view.KeyEvent.ACTION_DOWN, android.view.KeyEvent.KEYCODE_ENTER));
                ic.sendKeyEvent(new android.view.KeyEvent(android.view.KeyEvent.ACTION_UP, android.view.KeyEvent.KEYCODE_ENTER));
                return;
            }
            int imeOptions = editorInfo.imeOptions;
            int action = imeOptions & android.view.inputmethod.EditorInfo.IME_MASK_ACTION;
            boolean noEnterAction = (imeOptions & android.view.inputmethod.EditorInfo.IME_FLAG_NO_ENTER_ACTION) != 0;

            // If the editor flags IME_FLAG_NO_ENTER_ACTION (e.g. multi-line fields in
            // messaging apps like Signal), or if there's no meaningful action, insert a
            // newline. Otherwise perform the editor action (Go, Search, Send, etc.).
            if (!noEnterAction && (
                    action == android.view.inputmethod.EditorInfo.IME_ACTION_GO ||
                    action == android.view.inputmethod.EditorInfo.IME_ACTION_SEARCH ||
                    action == android.view.inputmethod.EditorInfo.IME_ACTION_SEND ||
                    action == android.view.inputmethod.EditorInfo.IME_ACTION_NEXT)) {
                ic.performEditorAction(action);
            } else {
                ic.sendKeyEvent(new android.view.KeyEvent(android.view.KeyEvent.ACTION_DOWN, android.view.KeyEvent.KEYCODE_ENTER));
                ic.sendKeyEvent(new android.view.KeyEvent(android.view.KeyEvent.ACTION_UP, android.view.KeyEvent.KEYCODE_ENTER));
            }
        }
    }

    private void updateRecordButtonUI(boolean recording) {
        isRecording = recording;
        // Keep the screen awake while recording so it never sleeps mid-capture
        // and cuts the recording short. Cleared automatically once we stop.
        if (inputView != null) {
            inputView.setKeepScreenOn(recording);
        }
        tintRecordButton(recording);
        if (recording) {
            statusView.setText("Listening...");
            hintView.setText("Tap to Stop · hold to cancel");
        } else {
            statusView.setText("Processing...");
            hintView.setText("Tap to Record");
            if (micLevelView != null) micLevelView.setLevel(0f);
        }
    }

    /** Tints the round record button + mic: idle = primary, recording = error. */
    private void tintRecordButton(boolean recording) {
        int circleAttr = recording
                ? com.google.android.material.R.attr.colorPrimary
                : com.google.android.material.R.attr.colorPrimaryContainer;
        int iconAttr = recording
                ? com.google.android.material.R.attr.colorOnPrimary
                : com.google.android.material.R.attr.colorOnPrimaryContainer;
        if (recordCircle != null) {
            recordCircle.setBackgroundTintList(ColorStateList.valueOf(
                    MaterialColors.getColor(recordCircle, circleAttr)));
        }
        if (micIcon != null) {
            micIcon.setColorFilter(MaterialColors.getColor(micIcon, iconAttr));
        }
    }

    @Override
    public void onDestroy() {
        super.onDestroy();
        cleanupNative();
        if (pauseAudioActive) {
            audioPauser.abandon(this);
            pauseAudioActive = false;
        }
    }

    // Native methods
    private native void initNative(RustInputMethodService service);
    private native void cleanupNative();
    private native void startRecording();
    private native void stopRecording();
    private native void cancelRecording();

    // Called from Rust
    public void onStatusUpdate(String status) {
        mainHandler.post(() -> {
            Log.d(TAG, "Status: " + status);
            lastStatus = status;
            boolean isError = status != null && status.startsWith("Error");
            if (status != null && status.startsWith("Ready")) {
                modelReady = true;
                if (pendingAutoStart && windowVisible && !isRecording) {
                    pendingAutoStart = false;
                    beginRecording();
                }
            } else if (isError && isRecording) {
                // The streaming consumer died without a final transcript
                // (e.g. buffer overflow while the model loaded). Reset the
                // session instead of dangling at "Listening..." with a hot
                // mic: close the mic, clear the composing tail, restore the
                // idle UI (the error text is shown by updateUiState).
                try {
                    stopRecording();
                } catch (Throwable t) {
                    Log.w(TAG, "stopRecording after error failed", t);
                }
                committedBase = "";
                lastTentative = "";
                pendingSend = false;
                InputConnection ic = getCurrentInputConnection();
                if (ic != null) ic.finishComposingText();
                updateRecordButtonUI(false);
            }
            updateUiState();
            if (pendingSwitchBack && isError) {
                pendingSwitchBack = false;
                switchToPreviousInputMethod();
            }
            if (pauseAudioActive && isError) {
                audioPauser.abandon(this);
                pauseAudioActive = false;
            }
        });
    }

    private void updateUiState() {
        boolean isLoading = lastStatus.contains("Loading") || lastStatus.contains("Initializing")
                || lastStatus.contains("Checking");
        boolean isWaiting = lastStatus.contains("Waiting");
        boolean isTranscribing = lastStatus.contains("Transcribing")
                || lastStatus.contains("Processing")
                || lastStatus.contains("Finishing");
        boolean isError = lastStatus.startsWith("Error");

        if (statusView != null && !isRecording) {
            if (isError) {
                statusView.setText(lastStatus);
            } else if (!modelReady) {
                // Real model-load state instead of a misleading "Tap to
                // Record" while the engine is still loading.
                statusView.setText("Loading model…");
            } else if (isTranscribing || isWaiting) {
                statusView.setText("Processing...");
            } else {
                statusView.setText("Tap to Record");
            }
        }

        // Hide progress bar - don't expose model loading to user
        if (progressBar != null) {
            progressBar.setVisibility(View.GONE);
        }

        // The record button is unusable while the model loads (recording
        // would race the load), during transcription, and on fatal errors —
        // but stays tappable after a load error so the user can retry.
        if (recordContainer != null) {
            boolean disable = isTranscribing || isWaiting
                    || (!modelReady && !isError);
            recordContainer.setEnabled(!disable);
            recordContainer.setAlpha(disable ? 0.5f : 1.0f);
        }

        if (hintView != null && !isRecording) {
            hintView.setText(isLoading && !modelReady ? "Loading model…"
                    : isError ? lastStatus : "Tap to Record");
        }
    }

    // Called from Rust during streaming: `committed` is the append-only
    // UI-stable prefix, `tentative` the still-revisable tail. The stable
    // delta is committed (commitText replaces any active composing region —
    // i.e. the previous tentative tail), then the new tail becomes the
    // composing region so it can still be refined by later updates. The text
    // itself is the preview — it streams into the focused field, so the IME
    // status row stays a state indicator ("Listening…") and shows no
    // transcription echo.
    public void onPartialText(String committed, String tentative) {
        mainHandler.post(() -> {
            if (!isRecording) return;
            InputConnection ic = getCurrentInputConnection();
            if (inputActive && ic != null) {
                if (committed != null && committed.length() > committedBase.length()) {
                    ic.commitText(committed.substring(committedBase.length()), 1);
                    committedBase = committed;
                }
                lastTentative = tentative == null ? "" : tentative;
                ic.setComposingText(lastTentative, 1);
            }
        });
    }

    // Called from Rust when the stream is finalized. With streaming, most of
    // the text is usually already committed via onPartialText; only the
    // remainder (plus the trailing space) is committed here.
    public void onTextTranscribed(String text) {
        mainHandler.post(() -> {
            String finalText = text == null ? "" : text.trim();
            if (finalText.isEmpty()) {
                // Nothing recognized — clear any composing tail, don't insert
                // a stray space.
                InputConnection ic = getCurrentInputConnection();
                if (ic != null) ic.finishComposingText();
                committedBase = "";
                lastTentative = "";
                updateRecordButtonUI(false);
                if (statusView != null) statusView.setText("Tap to Record");
                if (pauseAudioActive) {
                    audioPauser.abandon(this);
                    pauseAudioActive = false;
                }
                if (pendingSend) {
                    // Nothing recognized, but the return key was pressed:
                    // still perform the enter action — the user may be
                    // sending text they typed themselves.
                    pendingSend = false;
                    performEnterAction();
                }
                if (pendingSwitchBack || isSwitchBackEnabled()) {
                    // Nothing recognized still counts as "dictation ended":
                    // with auto switch-back, return to the previous keyboard.
                    pendingSwitchBack = false;
                    switchToPreviousInputMethod();
                }
                return;
            }
            InputConnection ic = getCurrentInputConnection();
            if (inputActive && ic != null) {
                // Commit the not-yet-committed remainder (commitText also
                // replaces the composing region), then the trailing space,
                // keeping the batch-era convention of a space after each
                // utterance. transcribe.cpp guarantees the streamed committed
                // prefix is append-only, but the final full text can still
                // disagree with it — a late revision of already-stable text,
                // or just the trim() on the Rust side eating a trailing space
                // the committer had already sent. So the split point is the
                // real common prefix, not committedBase.length().
                int common = commonPrefixLength(finalText, committedBase);
                if (common == committedBase.length()) {
                    String remainder = finalText.substring(common);
                    if (!remainder.isEmpty()) {
                        ic.commitText(remainder, 1);
                    } else {
                        ic.finishComposingText();
                    }
                } else {
                    repairCommittedPrefix(ic, finalText, common);
                }
                ic.commitText(" ", 1);

                if (!pendingSwitchBack && !pendingSend
                        && new File(getFilesDir(), "select_transcription").exists()) {
                    android.view.inputmethod.ExtractedText et = ic.getExtractedText(
                            new android.view.inputmethod.ExtractedTextRequest(), 0);
                    if (et != null) {
                        int end = et.selectionStart;
                        int start = end - (finalText.length() + 1);
                        if (start >= 0) {
                            ic.setSelection(start, end);
                        }
                    }
                }
            } else {
                // No editor is focused right now (the field dropped focus
                // mid-dictation). Committing now would be silently dropped, so
                // defer until a field is focused again instead of losing the
                // text. The old field still holds everything we streamed —
                // committedBase, plus the composing tail the editor finalized
                // into plain text on focus loss — so defer only the remainder
                // past that, or the refocus duplicates it.
                String streamed = committedBase + lastTentative;
                int common = commonPrefixLength(finalText, streamed);
                if (common == streamed.length()) {
                    String remainder = finalText.substring(common);
                    if (!remainder.isEmpty()) {
                        pendingCommitText = remainder + " ";
                    }
                    // Else the whole final text is already in the old field;
                    // deferring a lone space would double-space on refocus.
                } else {
                    // The final text revised streamed text the unfocused field
                    // already holds. Those stale bytes are stuck (no
                    // connection, unknown cursor position), and appending a
                    // mismatched tail would corrupt the field — leave the
                    // streamed text rather than mangle it.
                    Log.w(TAG, "final transcript disagrees with streamed text; "
                            + "deferred commit skipped");
                }
                // No live editor to send to; a queued return fires into the
                // void, so drop it rather than surprise-send on a later
                // refocus.
                pendingSend = false;
            }
            committedBase = "";
            lastTentative = "";
            if (pauseAudioActive) {
                audioPauser.abandon(this);
                pauseAudioActive = false;
            }
            updateRecordButtonUI(false);
            if (statusView != null) statusView.setText("Tap to Record");
            // Return key pressed mid-recording: the text has landed, now
            // perform the enter action (e.g. IME_ACTION_SEND) before any
            // switch-back hands the keyboard away.
            if (pendingSend) {
                pendingSend = false;
                performEnterAction();
            }
            // After a successful transcription, hand the keyboard back to
            // whatever the user was typing on before (unless they chose to
            // keep this keyboard open) — or immediately when they hit the
            // switch key mid-recording (pendingSwitchBack).
            if (pendingSwitchBack || isSwitchBackEnabled()) {
                pendingSwitchBack = false;
                switchToPreviousInputMethod();
            }
        });
    }

    // Commits text that finished transcribing while no field was focused. Called
    // from onStartInputView when an editor (and a live input connection) is
    // available again.
    private void flushPendingText() {
        if (pendingCommitText == null) return;
        InputConnection ic = getCurrentInputConnection();
        if (ic != null) {
            ic.commitText(pendingCommitText, 1);
            pendingCommitText = null;
        }
    }

    /** Length of the longest common prefix of a and b. */
    private static int commonPrefixLength(String a, String b) {
        int n = Math.min(a.length(), b.length());
        int i = 0;
        while (i < n && a.charAt(i) == b.charAt(i)) i++;
        return i;
    }

    /** The final transcript shrank or rewrote text the field already holds
     *  relative to the streamed committed prefix (committedBase[common..] is
     *  no longer part of the final text). Committed text can't be un-committed,
     *  so repair in place: drop the composing tail, delete the stale committed
     *  bytes, and commit the corrected suffix. The delete is verified against
     *  the actual field contents first — if the user moved the cursor or typed
     *  around our commits, leave the streamed text rather than eat theirs. */
    private void repairCommittedPrefix(InputConnection ic, String finalText, int common) {
        int excess = committedBase.length() - common;
        // Remove the (stale) tentative tail without committing it, so the
        // cursor lands right after the committed prefix.
        ic.setComposingText("", 1);
        CharSequence before = ic.getTextBeforeCursor(excess, 0);
        if (before != null && before.length() == excess
                && committedBase.substring(common).contentEquals(before)) {
            ic.deleteSurroundingText(excess, 0);
            String corrected = finalText.substring(common);
            if (!corrected.isEmpty()) {
                ic.commitText(corrected, 1);
            } else {
                ic.finishComposingText();
            }
        } else {
            Log.w(TAG, "final transcript disagrees with committed prefix; "
                    + "leaving streamed text in field");
            ic.finishComposingText();
        }
    }

    public void onAudioLevel(float level) {
        if (micLevelView != null) {
            mainHandler.post(() -> micLevelView.setLevel(level));
        }
    }

    private boolean isPauseAudioEnabled() {
        return new File(getFilesDir(), "pause_audio").exists();
    }

    /** "Record in background" is default ON; the marker file is the opt-out. */
    private boolean isStopOnHideEnabled() {
        return new File(getFilesDir(), "stop_on_hide").exists();
    }

    /**
     * Return to the previous keyboard after a successful transcription.
     * Default ON (the marker file is the opt-out) — the dictation flow most
     * users want is: speak, text lands, previous keyboard returns.
     */
    private boolean isSwitchBackEnabled() {
        return !new File(getFilesDir(), "keep_keyboard_open").exists();
    }

    /** Floating-keyboard mode is a plain marker file (default off; the main
     *  app enables it by default on tablet-class screens at first run). */
    private boolean isFloatingKeyboardEnabled() {
        return new File(getFilesDir(), "floating_keyboard").exists();
    }

    // --- Floating keyboard window -------------------------------------------

    private static final String PREFS_NAME = "ime_prefs";
    private static final String KEY_FLOAT_X = "float_x";
    private static final String KEY_FLOAT_Y = "float_y";

    private static int clamp(int v, int lo, int hi) {
        return Math.max(lo, Math.min(hi, v));
    }

    /** Bounds of the display the IME window is on (rotation-aware). */
    private android.graphics.Rect displayBounds() {
        android.graphics.Rect r = new android.graphics.Rect();
        Object wm = getSystemService(Context.WINDOW_SERVICE);
        if (wm instanceof android.view.WindowManager) {
            android.view.WindowManager wmm = (android.view.WindowManager) wm;
            if (android.os.Build.VERSION.SDK_INT >= 30) {
                r.set(wmm.getCurrentWindowMetrics().getBounds());
            } else {
                android.graphics.Point p = new android.graphics.Point();
                wmm.getDefaultDisplay().getRealSize(p);
                r.set(0, 0, p.x, p.y);
            }
        }
        if (r.isEmpty()) {
            r.set(0, 0, getResources().getDisplayMetrics().widthPixels,
                    getResources().getDisplayMetrics().heightPixels);
        }
        return r;
    }

    /** Panel height for clamping: the measured view once laid out, else an
     *  estimate (half the display) so the first show is still on-screen. */
    private int floatingPanelHeight() {
        if (inputView != null && inputView.getHeight() > 0) return inputView.getHeight();
        return Math.max(1, displayBounds().height() / 2);
    }

    /**
     * The framework lays the IME window out through here on show and
     * fullscreen-mode transitions; the default forces MATCH_PARENT x
     * WRAP_CONTENT, which clobbers the floating geometry. Route both modes
     * through our own params instead.
     */
    @Override
    public void onConfigureWindow(android.view.Window win, boolean isFullscreen,
            boolean isCandidatesOnly) {
        if (floatingMode) {
            applyFloatingWindow();
        } else {
            applyDockedWindow();
        }
    }

    @Override
    public void onConfigurationChanged(android.content.res.Configuration newConfig) {
        super.onConfigurationChanged(newConfig);
        // Rotation changed the display bounds; re-clamp the saved position.
        if (floatingMode) applyFloatingWindow();
    }

    /** Turns the IME window into a wrap-content, positionable panel. The
     *  restored position is clamped so the panel is always fully on-screen. */
    private void applyFloatingWindow() {
        android.view.Window window = getWindow().getWindow();
        if (window == null) return;
        float density = getResources().getDisplayMetrics().density;
        android.graphics.Rect bounds = displayBounds();
        floatingWidthPx = (int) (340 * density); // ~340dp panel; keys wrap within it
        if (floatingWidthPx > bounds.width()) floatingWidthPx = bounds.width();

        android.content.SharedPreferences prefs =
                getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE);
        android.view.WindowManager.LayoutParams lp = window.getAttributes();
        lp.width = floatingWidthPx;
        lp.height = android.view.WindowManager.LayoutParams.WRAP_CONTENT;
        lp.gravity = android.view.Gravity.TOP | android.view.Gravity.START;
        lp.x = clamp(prefs.getInt(KEY_FLOAT_X, 16 * (int) density),
                0, Math.max(0, bounds.width() - floatingWidthPx));
        lp.y = clamp(prefs.getInt(KEY_FLOAT_Y, 96 * (int) density),
                0, Math.max(0, bounds.height() - floatingPanelHeight()));
        window.setAttributes(lp);
    }

    /** Docked slab: full width at the bottom, no offsets. */
    private void applyDockedWindow() {
        android.view.Window window = getWindow().getWindow();
        if (window == null) return;
        android.view.WindowManager.LayoutParams lp = window.getAttributes();
        lp.width = android.view.WindowManager.LayoutParams.MATCH_PARENT;
        lp.height = android.view.WindowManager.LayoutParams.WRAP_CONTENT;
        lp.gravity = android.view.Gravity.BOTTOM;
        lp.x = 0;
        lp.y = 0;
        window.setAttributes(lp);
    }

    /** Drag-to-move for the floating panel: the handle moves the IME window,
     *  the position stays clamped on-screen, and it is remembered across
     *  sessions. */
    private void attachDragHandler(View dragHandle) {
        dragHandle.setOnTouchListener((v, event) -> {
            android.view.Window window = getWindow().getWindow();
            if (window == null) return false;
            final android.view.WindowManager.LayoutParams lp = window.getAttributes();
            switch (event.getActionMasked()) {
                case android.view.MotionEvent.ACTION_DOWN:
                    v.setTag(new float[]{event.getRawX(), event.getRawY(), lp.x, lp.y});
                    return true;
                case android.view.MotionEvent.ACTION_MOVE: {
                    float[] start = (float[]) v.getTag();
                    if (start == null) return true;
                    android.graphics.Rect bounds = displayBounds();
                    lp.x = clamp((int) (start[2] + event.getRawX() - start[0]),
                            0, Math.max(0, bounds.width() - floatingWidthPx));
                    lp.y = clamp((int) (start[3] + event.getRawY() - start[1]),
                            0, Math.max(0, bounds.height() - floatingPanelHeight()));
                    window.setAttributes(lp);
                    return true;
                }
                case android.view.MotionEvent.ACTION_UP:
                case android.view.MotionEvent.ACTION_CANCEL: {
                    getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                            .edit()
                            .putInt(KEY_FLOAT_X, lp.x)
                            .putInt(KEY_FLOAT_Y, lp.y)
                            .apply();
                    v.performClick();
                    return true;
                }
            }
            return false;
        });
    }
}
