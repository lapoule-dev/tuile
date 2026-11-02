// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

package dev.lapoule.tuile;

import android.app.AlertDialog;
import android.app.NativeActivity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.database.Cursor;
import android.graphics.Color;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.PowerManager;
import android.provider.OpenableColumns;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.util.Base64;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowInsets;
import android.widget.PopupWindow;
import android.widget.TextView;

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.security.KeyStore;

import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;

/**
 * The viewer's activity: the platform's own native activity, which hands the
 * window, its input and its lifecycle to the Rust application, plus the few
 * things a native thread cannot do by itself on this system.
 *
 * <ul>
 *   <li>Memory pressure arrives as {@code onTrimMemory}, which the native
 *       activity does not forward. It is counted here, and the application
 *       reads the count and the level once a second — no native method is
 *       registered, so no symbol is exported for it.
 *   <li>Views (the caption, an alert) belong to the UI thread: the
 *       application calls these methods from its own thread and they post the
 *       work there.
 *   <li>The temperature, the battery saver and the launch's settings are read
 *       through the system's services.
 *   <li>A configuration file opened or shared with the application, or an
 *       import link, is held here as it arrives ({@code onCreate},
 *       {@code onNewIntent}) and taken by the application once a second
 *       ({@link #takeImport}); whether it is any good is decided in Rust.
 *   <li>The access token it brings is sealed with a key that lives in the
 *       Android Keystore ({@link #sealSecret}, {@link #openSecret}).
 * </ul>
 *
 * Nothing here decides anything: every policy — what a trim level means, what
 * to let go of, which launch settings to accept — is in Rust, and tested there.
 */
public class TuileActivity extends NativeActivity {
    /** How many times the system has said it is short of memory, and the last level it said. */
    private static volatile int trimCount = 0;
    private static volatile int trimLevel = 0;

    private PopupWindow caption;
    private TextView captionText;
    /** The last caption asked for: shown once the window can carry it. UI thread only. */
    private String captionWanted = "";
    private boolean captionRetrying = false;
    /** How far the caption stands off the bottom, pixels: the credits bar's height. UI thread only. */
    private int captionLift = 0;

    /** The largest configuration file read: a configuration is a few hundred bytes. */
    private static final int LONGEST_FILE = 64 * 1024;
    /** The Keystore key the secrets are sealed with, and where the sealed values are kept. */
    private static final String SECRET_KEY = "tuile-secrets";
    private static final String SECRET_STORE = "tuile-secrets";

    /** A configuration that arrived and is not taken yet: see {@link #takeImport}. */
    private static String pendingImport = null;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // Not again when the system recreates the activity: the intent was
        // taken the first time.
        if (savedInstanceState == null) {
            receive(getIntent());
        }
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        receive(intent);
    }

    /**
     * What a view or a share brought: an import link ({@code link\n<url>}),
     * or a file's name and text ({@code file\n<name>\n<text>}), read off the
     * UI thread — a provider may be remote.
     */
    private void receive(final Intent intent) {
        if (intent == null) {
            return;
        }
        String action = intent.getAction();
        Uri uri = null;
        if (Intent.ACTION_VIEW.equals(action)) {
            uri = intent.getData();
        } else if (Intent.ACTION_SEND.equals(action)) {
            Object stream = intent.getParcelableExtra(Intent.EXTRA_STREAM);
            if (stream instanceof Uri) {
                uri = (Uri) stream;
            } else {
                CharSequence text = intent.getCharSequenceExtra(Intent.EXTRA_TEXT);
                if (text != null && text.length() <= LONGEST_FILE) {
                    hold("file\nshared text\n" + text);
                }
                return;
            }
        }
        if (uri == null) {
            return;
        }
        String scheme = uri.getScheme();
        if (!"content".equals(scheme) && !"file".equals(scheme)) {
            // The application's own scheme: the link, as it came.
            hold("link\n" + uri.toString());
            return;
        }
        final Uri file = uri;
        new Thread(new Runnable() {
            @Override
            public void run() {
                String text = read(file);
                if (text != null) {
                    hold("file\n" + displayName(file) + "\n" + text);
                }
            }
        }, "tuile-import").start();
    }

    private static synchronized void hold(String what) {
        pendingImport = what;
    }

    /** The configuration that arrived since the last call, or null. */
    public String takeImport() {
        synchronized (TuileActivity.class) {
            String taken = pendingImport;
            pendingImport = null;
            return taken;
        }
    }

    private String read(Uri uri) {
        try (InputStream in = getContentResolver().openInputStream(uri)) {
            if (in == null) {
                return null;
            }
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] chunk = new byte[8192];
            int n;
            while ((n = in.read(chunk)) > 0) {
                out.write(chunk, 0, n);
                if (out.size() > LONGEST_FILE) {
                    // Too large to be a configuration: Rust says so.
                    break;
                }
            }
            return new String(out.toByteArray(), StandardCharsets.UTF_8);
        } catch (Exception e) {
            return null;
        }
    }

    private String displayName(Uri uri) {
        if ("content".equals(uri.getScheme())) {
            try (Cursor cursor = getContentResolver().query(
                    uri, new String[] {OpenableColumns.DISPLAY_NAME}, null, null, null)) {
                if (cursor != null && cursor.moveToFirst()) {
                    String name = cursor.getString(0);
                    if (name != null && !name.isEmpty()) {
                        return name.replace('\n', ' ');
                    }
                }
            } catch (Exception e) {
                // The address's last part, below.
            }
        }
        String last = uri.getLastPathSegment();
        return last == null ? "the configuration" : last.replace('\n', ' ');
    }

    /** The Keystore's key for the secrets, made the first time. */
    private static SecretKey secretKey() throws Exception {
        KeyStore store = KeyStore.getInstance("AndroidKeyStore");
        store.load(null);
        if (store.containsAlias(SECRET_KEY)) {
            return ((KeyStore.SecretKeyEntry) store.getEntry(SECRET_KEY, null)).getSecretKey();
        }
        KeyGenerator generator =
                KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore");
        generator.init(new KeyGenParameterSpec.Builder(
                        SECRET_KEY, KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build());
        return generator.generateKey();
    }

    /** Seals {@code value} under {@code name}: AES-GCM, the key in the Keystore. */
    public boolean sealSecret(String name, String value) {
        try {
            Cipher cipher = Cipher.getInstance("AES/GCM/NoPadding");
            cipher.init(Cipher.ENCRYPT_MODE, secretKey());
            byte[] iv = cipher.getIV();
            byte[] sealed = cipher.doFinal(value.getBytes(StandardCharsets.UTF_8));
            byte[] both = new byte[iv.length + sealed.length];
            System.arraycopy(iv, 0, both, 0, iv.length);
            System.arraycopy(sealed, 0, both, iv.length, sealed.length);
            return getSharedPreferences(SECRET_STORE, Context.MODE_PRIVATE)
                    .edit()
                    .putString(name, Base64.encodeToString(both, Base64.NO_WRAP))
                    .commit();
        } catch (Exception e) {
            return false;
        }
    }

    /** The value sealed under {@code name}, or null. */
    public String openSecret(String name) {
        SharedPreferences store = getSharedPreferences(SECRET_STORE, Context.MODE_PRIVATE);
        String kept = store.getString(name, null);
        if (kept == null) {
            return null;
        }
        try {
            byte[] both = Base64.decode(kept, Base64.NO_WRAP);
            Cipher cipher = Cipher.getInstance("AES/GCM/NoPadding");
            cipher.init(Cipher.DECRYPT_MODE, secretKey(), new GCMParameterSpec(128, both, 0, 12));
            byte[] open = cipher.doFinal(both, 12, both.length - 12);
            return new String(open, StandardCharsets.UTF_8);
        } catch (Exception e) {
            return null;
        }
    }

    @Override
    public void onTrimMemory(int level) {
        super.onTrimMemory(level);
        trimLevel = level;
        trimCount = trimCount + 1;
    }

    /** The last trim level the system sent, in the low 32 bits; how many it has sent, above. */
    public long trimSignal() {
        return ((long) trimCount << 32) | (trimLevel & 0xffffffffL);
    }

    /** PowerManager's thermal status, 0 (none) to 6 (shutdown); 0 before Android 10. */
    public int thermalStatus() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            return 0;
        }
        PowerManager power = (PowerManager) getSystemService(Context.POWER_SERVICE);
        return power == null ? 0 : power.getCurrentThermalStatus();
    }

    /** Whether the battery saver is on. */
    public boolean powerSave() {
        PowerManager power = (PowerManager) getSystemService(Context.POWER_SERVICE);
        return power != null && power.isPowerSaveMode();
    }

    /**
     * What the system's bars and the display's cutout cover of the window, in
     * pixels: left, top, right and bottom, sixteen bits each from the low
     * end. The window is drawn edge to edge (the rule from Android 15 on), so
     * the controls keep inside these. -1 while the window has no insets yet.
     */
    public long safeInsets() {
        View decor = getWindow().getDecorView();
        WindowInsets insets = decor.getRootWindowInsets();
        if (insets == null) {
            return -1L;
        }
        int left;
        int top;
        int right;
        int bottom;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            android.graphics.Insets bars = insets.getInsets(
                    WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
            left = bars.left;
            top = bars.top;
            right = bars.right;
            bottom = bars.bottom;
        } else {
            left = insets.getSystemWindowInsetLeft();
            top = insets.getSystemWindowInsetTop();
            right = insets.getSystemWindowInsetRight();
            bottom = insets.getSystemWindowInsetBottom();
        }
        return (left & 0xffffL)
                | ((top & 0xffffL) << 16)
                | ((right & 0xffffL) << 32)
                | ((bottom & 0xffffL) << 48);
    }

    /** The application's cache directory, which the system may clear. */
    public String cacheDirectory() {
        return getCacheDir().getAbsolutePath();
    }

    /** The application's private files directory. */
    public String filesDirectory() {
        return getFilesDir().getAbsolutePath();
    }

    /**
     * The launch intent's string extras, one {@code KEY=VALUE} a line. Which of
     * them the application accepts is decided in Rust.
     */
    public String launchSettings() {
        Intent intent = getIntent();
        Bundle extras = intent == null ? null : intent.getExtras();
        if (extras == null) {
            return "";
        }
        StringBuilder lines = new StringBuilder();
        for (String key : extras.keySet()) {
            Object value = extras.get(key);
            if (value instanceof String && ((String) value).indexOf('\n') < 0) {
                lines.append(key).append('=').append((String) value).append('\n');
            }
        }
        return lines.toString();
    }

    /** A line of text over the bottom of the view: the attribution and the last notice. */
    public void caption(final String text) {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                showCaption(text);
            }
        });
    }

    /** Lifts the caption off the bottom by the credits bar's height, in pixels. */
    public void captionLift(final int pixels) {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                captionLift = pixels;
                if (caption != null && caption.isShowing()) {
                    caption.update(0, captionLift, -1, -1);
                }
            }
        });
    }

    private void showCaption(String text) {
        captionWanted = text;
        final View anchor = getWindow().getDecorView();
        if (anchor.getWindowToken() == null) {
            // Asked before the window is attached — the attribution is set
            // once, at launch: tried again shortly, with whatever is wanted
            // by then. One retry in flight at a time.
            if (!captionRetrying) {
                captionRetrying = true;
                anchor.postDelayed(new Runnable() {
                    @Override
                    public void run() {
                        captionRetrying = false;
                        if (!isFinishing()) {
                            showCaption(captionWanted);
                        }
                    }
                }, 250);
            }
            return;
        }
        if (caption == null) {
            captionText = new TextView(this);
            captionText.setTextColor(Color.WHITE);
            captionText.setShadowLayer(3f, 0f, 1f, Color.BLACK);
            captionText.setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f);
            captionText.setGravity(Gravity.CENTER);
            int pad = (int) TypedValue.applyDimension(
                    TypedValue.COMPLEX_UNIT_DIP, 6f, getResources().getDisplayMetrics());
            captionText.setPadding(pad, pad, pad, pad);
            caption = new PopupWindow(captionText,
                    ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
            // The globe keeps every touch: the caption is only drawn over it.
            caption.setTouchable(false);
            caption.setFocusable(false);
            caption.setClippingEnabled(true);
        }
        captionText.setText(text);
        if (text.isEmpty()) {
            caption.dismiss();
        } else if (!caption.isShowing()) {
            caption.showAtLocation(anchor, Gravity.BOTTOM | Gravity.CENTER_HORIZONTAL, 0, captionLift);
        }
    }

    @Override
    protected void onDestroy() {
        if (caption != null) {
            caption.dismiss();
        }
        super.onDestroy();
    }

    /**
     * A message over the view, with one button. Returns whether it can be
     * shown now: not while the activity is finishing or has no window yet.
     */
    public boolean alert(final String title, final String text) {
        if (isFinishing() || getWindow().getDecorView().getWindowToken() == null) {
            return false;
        }
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (isFinishing()) {
                    return;
                }
                new AlertDialog.Builder(TuileActivity.this)
                        .setTitle(title)
                        .setMessage(text)
                        .setPositiveButton(android.R.string.ok, null)
                        .show();
            }
        });
        return true;
    }

    /** Puts a line of text on the clipboard. */
    public boolean copy(String text) {
        ClipboardManager clipboard = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        if (clipboard == null) {
            return false;
        }
        clipboard.setPrimaryClip(ClipData.newPlainText("view", text));
        return true;
    }

    /** The text on the clipboard, or null. */
    public String paste() {
        ClipboardManager clipboard = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        if (clipboard == null || !clipboard.hasPrimaryClip()) {
            return null;
        }
        ClipData clip = clipboard.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) {
            return null;
        }
        CharSequence text = clip.getItemAt(0).coerceToText(this);
        return text == null ? null : text.toString();
    }
}
