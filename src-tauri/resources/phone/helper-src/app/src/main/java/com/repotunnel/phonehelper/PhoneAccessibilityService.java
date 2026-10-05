package com.repotunnel.phonehelper;

import android.accessibilityservice.AccessibilityService;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.graphics.Rect;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.net.Uri;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public final class PhoneAccessibilityService extends AccessibilityService {
    static final String SOCKET_NAME = "repotunnel_phone_semantic_v1";
    private static final int MAX_REQUEST_BYTES = 256 * 1024;
    private static final int MAX_RESPONSE_BYTES = 4 * 1024 * 1024;
    private static volatile PhoneAccessibilityService instance;

    private final AtomicBoolean running = new AtomicBoolean(false);
    private final AtomicBoolean paymentSafeMode = new AtomicBoolean(false);
    private final AtomicBoolean paymentAppForeground = new AtomicBoolean(false);
    private final AtomicLong generation = new AtomicLong(1);
    private final Object snapshotLock = new Object();
    private final ThreadPoolExecutor requestExecutor = new ThreadPoolExecutor(
            2,
            4,
            30L,
            TimeUnit.SECONDS,
            new ArrayBlockingQueue<>(16),
            runnable -> {
                Thread thread = new Thread(runnable, "RepoTunnelPhoneRequestWorker");
                thread.setDaemon(true);
                return thread;
            },
            new ThreadPoolExecutor.AbortPolicy());
    private final ThreadPoolExecutor semanticExecutor = new ThreadPoolExecutor(
            1,
            1,
            30L,
            TimeUnit.SECONDS,
            new ArrayBlockingQueue<>(16),
            runnable -> {
                Thread thread = new Thread(runnable, "RepoTunnelPhoneSemanticWorker");
                thread.setDaemon(true);
                return thread;
            },
            new ThreadPoolExecutor.AbortPolicy());
    private volatile LocalServerSocket serverSocket;
    private volatile Thread serverThread;
    private String currentSnapshotId;
    private Map<String, Target> currentTargets = new HashMap<>();
    private final Map<String, Boolean> paymentPackageCache = new HashMap<>();

    private static final class Target {
        final int windowId;
        final int[] path;
        final String signature;
        final boolean sensitive;
        final boolean editable;
        final boolean clickable;

        Target(
                int windowId,
                int[] path,
                String signature,
                boolean sensitive,
                boolean editable,
                boolean clickable) {
            this.windowId = windowId;
            this.path = path;
            this.signature = signature;
            this.sensitive = sensitive;
            this.editable = editable;
            this.clickable = clickable;
        }
    }

    private static final class PendingNode {
        final AccessibilityNodeInfo node;
        final int windowId;
        final int[] path;
        final String parentBackendId;

        PendingNode(
                AccessibilityNodeInfo node,
                int windowId,
                int[] path,
                String parentBackendId) {
            this.node = node;
            this.windowId = windowId;
            this.path = path;
            this.parentBackendId = parentBackendId;
        }
    }

    @Override
    protected void onServiceConnected() {
        instance = this;
        paymentSafeMode.set(false);

        AccessibilityNodeInfo root = getRootInActiveWindow();
        if (root != null) {
            try {
                if (isPaymentSensitivePackage(root.getPackageName())) {
                    blockPaymentApp();
                }
            } finally {
                root.recycle();
            }
        }

        startServer();
    }

    @Override
    public void onAccessibilityEvent(AccessibilityEvent event) {
        if (event == null) {
            return;
        }

        int type = event.getEventType();
        CharSequence eventPackage = event.getPackageName();
        boolean paymentSensitive = isPaymentSensitivePackage(eventPackage);

        if (type == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED) {
            String packageName = eventPackage == null ? "" : eventPackage.toString();
            if (paymentSensitive) {
                blockPaymentApp();
                return;
            }
            if (!"com.android.systemui".equals(packageName)
                    && !getPackageName().equals(packageName)) {
                paymentAppForeground.set(false);
            }
        } else if (paymentAppForeground.get() && paymentSensitive) {
            return;
        }
        boolean changesGrounding =
                type == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED
                        || type == AccessibilityEvent.TYPE_WINDOWS_CHANGED
                        || type == AccessibilityEvent.TYPE_WINDOW_CONTENT_CHANGED
                        || type == AccessibilityEvent.TYPE_VIEW_SCROLLED
                        || type == AccessibilityEvent.TYPE_VIEW_FOCUSED
                        || type == AccessibilityEvent.TYPE_VIEW_TEXT_CHANGED;
        if (!changesGrounding) {
            return;
        }

        generation.incrementAndGet();
        // Keep the last published targets until a newer snapshot is successfully
        // published. Actions re-resolve the stored window/path against the live
        // accessibility tree and compare its signature before mutating, so unrelated
        // accessibility events can be safely revalidated instead of invalidating every
        // ref immediately.
    }

    @Override
    public void onInterrupt() {
    }

    @Override
    public void onDestroy() {
        stopServer();
        requestExecutor.shutdownNow();
        semanticExecutor.shutdownNow();
        instance = null;
        super.onDestroy();
    }

    static void onTokenUpdated() {
        PhoneAccessibilityService service = instance;
        if (service != null && !service.paymentSafeMode.get()) {
            service.startServer();
        }
    }

    private boolean isPaymentSensitivePackage(CharSequence packageName) {
        if (packageName == null) {
            return false;
        }

        String value = packageName.toString().trim();
        if (value.isEmpty() || getPackageName().equals(value)) {
            return false;
        }

        synchronized (paymentPackageCache) {
            Boolean cached = paymentPackageCache.get(value);
            if (cached != null) {
                return cached;
            }
        }

        boolean sensitive = handlesUpiPayment(value) || hasPaymentIdentity(value);
        synchronized (paymentPackageCache) {
            if (paymentPackageCache.size() >= 256) {
                paymentPackageCache.clear();
            }
            paymentPackageCache.put(value, sensitive);
        }
        return sensitive;
    }

    private boolean handlesUpiPayment(String packageName) {
        Intent intent =
                new Intent(
                        Intent.ACTION_VIEW,
                        Uri.parse("upi://pay?pa=repotunnel%40upi&pn=RepoTunnel&am=1.00&cu=INR"));
        intent.addCategory(Intent.CATEGORY_BROWSABLE);

        try {
            List<ResolveInfo> handlers =
                    getPackageManager()
                            .queryIntentActivities(intent, PackageManager.MATCH_DEFAULT_ONLY);
            for (ResolveInfo handler : handlers) {
                if (handler.activityInfo != null
                        && packageName.equals(handler.activityInfo.packageName)) {
                    return true;
                }
            }
        } catch (SecurityException ignored) {
        }
        return false;
    }

    private boolean hasPaymentIdentity(String packageName) {
        String packageMaterial = packageName.toLowerCase(Locale.ROOT);
        String[] packageHints = {
            ".bank",
            "banking",
            "wallet",
            "payment",
            "payments",
            "phonepe",
            "paytm",
            "paypal",
            "gpay",
            "bhim",
            "razorpay",
            "cashapp",
            "venmo",
            "revolut"
        };
        for (String hint : packageHints) {
            if (packageMaterial.contains(hint)) {
                return true;
            }
        }

        try {
            ApplicationInfo application =
                    getPackageManager().getApplicationInfo(packageName, PackageManager.GET_META_DATA);
            CharSequence rawLabel = getPackageManager().getApplicationLabel(application);
            String label = rawLabel == null ? "" : rawLabel.toString().toLowerCase(Locale.ROOT);
            String normalized = " " + label.replaceAll("[^a-z0-9]+", " ").trim() + " ";
            String[] labelHints = {
                " bank ",
                " banking ",
                " wallet ",
                " payment ",
                " payments ",
                " upi ",
                " pay ",
                " paypal ",
                " paytm ",
                " phonepe ",
                " bhim ",
                " venmo ",
                " revolut "
            };
            for (String hint : labelHints) {
                if (normalized.contains(hint)) {
                    return true;
                }
            }
        } catch (PackageManager.NameNotFoundException | SecurityException ignored) {
        }
        return false;
    }

    private void blockPaymentApp() {
        if (!paymentAppForeground.compareAndSet(false, true)) {
            return;
        }
        generation.incrementAndGet();
        synchronized (snapshotLock) {
            currentSnapshotId = null;
            currentTargets = new HashMap<>();
        }
    }

    private JSONObject foregroundPolicy() throws JSONException {
        return new JSONObject()
                .put("ok", true)
                .put("paymentSensitive", paymentAppForeground.get());
    }

    private JSONObject paymentAppBlockedError() {
        return error(
                "PAYMENT_APP_BLOCKED",
                "RepoTunnel keeps Accessibility enabled but blocks AI inspection and control while a payment-sensitive app is foreground.");
    }

    private void enterPaymentSafeMode() {
        if (!paymentSafeMode.compareAndSet(false, true)) {
            return;
        }

        blockPaymentApp();
        stopServer();

        if (Looper.myLooper() == Looper.getMainLooper()) {
            disableSelf();
        } else {
            Handler main = new Handler(Looper.getMainLooper());
            main.post(this::disableSelf);
        }
    }

    private void startServer() {
        if (!running.compareAndSet(false, true)) {
            return;
        }

        serverThread = new Thread(() -> {
            try (LocalServerSocket localServer = new LocalServerSocket(SOCKET_NAME)) {
                serverSocket = localServer;
                while (running.get()) {
                    LocalSocket socket;
                    try {
                        socket = localServer.accept();
                    } catch (IOException error) {
                        if (running.get()) {
                            continue;
                        }
                        break;
                    }

                    final LocalSocket client = socket;
                    try {
                        requestExecutor.execute(() -> routeClient(client));
                    } catch (RejectedExecutionException ignored) {
                        closeClient(client);
                    }
                }
            } catch (IOException ignored) {
            } finally {
                serverSocket = null;
                running.set(false);
            }
        }, "RepoTunnelPhoneSemantic");
        serverThread.setDaemon(true);
        serverThread.start();
    }

    private void routeClient(LocalSocket client) {
        try {
            client.setReceiveBufferSize(MAX_REQUEST_BYTES);
            client.setSoTimeout(5_000);
            JSONObject request = readRequest(client.getInputStream());
            String op = request.optString("op", "");
            if ("snapshot".equals(op) || "action".equals(op)) {
                try {
                    semanticExecutor.execute(() -> respondToClient(client, request));
                } catch (RejectedExecutionException ignored) {
                    closeClient(client);
                }
                return;
            }
            respondToClient(client, request);
        } catch (Exception ignored) {
            closeClient(client);
        }
    }

    private void respondToClient(LocalSocket socket, JSONObject request) {
        try (LocalSocket client = socket) {
            JSONObject response = handleRequest(request);
            writeResponse(client.getOutputStream(), response);
        } catch (Exception ignored) {
            // Intentionally do not log request bodies or typed text.
        }
    }

    private static void closeClient(LocalSocket client) {
        try {
            client.close();
        } catch (IOException ignored) {
        }
    }

    private void stopServer() {
        running.set(false);
        LocalServerSocket socket = serverSocket;
        if (socket != null) {
            try {
                socket.close();
            } catch (IOException ignored) {
            }
        }
    }

    private JSONObject readRequest(InputStream input) throws IOException, JSONException {
        ByteArrayOutputStream buffer = new ByteArrayOutputStream();
        while (buffer.size() <= MAX_REQUEST_BYTES) {
            int value = input.read();
            if (value < 0 || value == '\n') {
                break;
            }
            buffer.write(value);
        }
        if (buffer.size() == 0 || buffer.size() > MAX_REQUEST_BYTES) {
            throw new IOException("invalid request size");
        }
        return new JSONObject(buffer.toString(StandardCharsets.UTF_8.name()));
    }

    private void writeResponse(OutputStream output, JSONObject response) throws IOException {
        byte[] encoded = (response.toString() + "\n").getBytes(StandardCharsets.UTF_8);
        if (encoded.length > MAX_RESPONSE_BYTES) {
            encoded = (error(
                    "RESPONSE_TOO_LARGE",
                    "Semantic response exceeded the bounded size.").toString() + "\n")
                    .getBytes(StandardCharsets.UTF_8);
        }
        output.write(encoded);
        output.flush();
    }

    private JSONObject handleRequest(JSONObject request) {
        String nonce = request.optString("nonce", "");
        if (!authorized(nonce)) {
            return error("UNAUTHORIZED", "RepoTunnel helper session nonce was rejected.");
        }

        String op = request.optString("op", "");
        try {
            switch (op) {
                case "ping":
                    return new JSONObject()
                            .put("ok", true)
                            .put("accessibilityReady", true)
                            .put("protocolVersion", 1);
                case "snapshot":
                    return snapshot(Math.max(20, Math.min(800, request.optInt("maxNodes", 400))));
                case "classify_payment_package":
                    return new JSONObject()
                            .put("ok", true)
                            .put(
                                    "paymentSensitive",
                                    isPaymentSensitivePackage(
                                            request.optString("packageName", "")));
                case "foreground_policy":
                    return foregroundPolicy();
                case "pause_for_payment":
                    enterPaymentSafeMode();
                    return new JSONObject()
                            .put("ok", true)
                            .put("paymentSafeMode", true)
                            .put("accessibilityDisabling", true);
                case "action":
                    return action(
                            request.optString("backendId", ""),
                            request.optString("action", ""),
                            request.has("text") ? request.optString("text", "") : null);
                default:
                    return error("INVALID_ARGUMENT", "Unknown helper operation.");
            }
        } catch (Throwable ignored) {
            return error("HELPER_ERROR", "Android accessibility operation failed.");
        }
    }

    private boolean authorized(String supplied) {
        String expected = getSharedPreferences(ConfigReceiver.PREFS, Context.MODE_PRIVATE)
                .getString(ConfigReceiver.NONCE_KEY, "");
        if (expected == null || expected.length() != 64 || supplied == null) {
            return false;
        }
        return MessageDigest.isEqual(
                expected.getBytes(StandardCharsets.UTF_8),
                supplied.toLowerCase(Locale.ROOT).getBytes(StandardCharsets.UTF_8));
    }

    private JSONObject snapshot(int maxNodes) throws JSONException {
        List<AccessibilityWindowInfo> windows = getWindows();
        List<PendingNode> roots = new ArrayList<>();

        if (windows != null) {
            for (AccessibilityWindowInfo window : windows) {
                AccessibilityNodeInfo root = window.getRoot();
                if (root != null) {
                    roots.add(new PendingNode(root, window.getId(), new int[0], null));
                }
            }
        }

        if (roots.isEmpty()) {
            AccessibilityNodeInfo root = getRootInActiveWindow();
            if (root != null) {
                roots.add(new PendingNode(root, root.getWindowId(), new int[0], null));
            }
        }

        if (roots.isEmpty()) {
            return error("SEMANTIC_UNAVAILABLE", "Android returned no accessibility root.");
        }

        for (PendingNode pending : roots) {
            if (isPaymentSensitivePackage(pending.node.getPackageName())) {
                for (PendingNode root : roots) {
                    root.node.recycle();
                }
                blockPaymentApp();
                return paymentAppBlockedError();
            }
        }

        long snapshotGeneration = generation.get();
        String snapshotId = "g" + snapshotGeneration;
        JSONArray nodes = new JSONArray();
        Map<String, Target> targets = new HashMap<>();
        ArrayDeque<PendingNode> queue = new ArrayDeque<>(roots);
        boolean truncated = false;
        int index = 0;

        while (!queue.isEmpty()) {
            if (index >= maxNodes) {
                truncated = true;
                while (!queue.isEmpty()) {
                    queue.removeFirst().node.recycle();
                }
                break;
            }

            PendingNode pending = queue.removeFirst();
            AccessibilityNodeInfo node = pending.node;
            String backendId = "h:" + snapshotGeneration + ":" + index;
            nodes.put(encodeNode(node, backendId, pending.parentBackendId));

            targets.put(
                    backendId,
                    new Target(
                            pending.windowId,
                            pending.path,
                            signature(node),
                            isSensitive(node),
                            node.isEditable(),
                            node.isClickable()));

            int childCount = Math.min(node.getChildCount(), 256);
            for (int childIndex = 0; childIndex < childCount; childIndex++) {
                AccessibilityNodeInfo child = node.getChild(childIndex);
                if (child == null) {
                    continue;
                }
                int[] path = Arrays.copyOf(pending.path, pending.path.length + 1);
                path[path.length - 1] = childIndex;
                queue.addLast(new PendingNode(child, pending.windowId, path, backendId));
            }

            node.recycle();
            index++;
        }

        if (generation.get() != snapshotGeneration) {
            return error(
                    "STALE_UI",
                    "Android UI changed while the semantic snapshot was being collected.");
        }

        synchronized (snapshotLock) {
            if (generation.get() != snapshotGeneration) {
                return error(
                        "STALE_UI",
                        "Android UI changed while the semantic snapshot was being published.");
            }
            currentSnapshotId = snapshotId;
            currentTargets = targets;
        }

        return new JSONObject()
                .put("ok", true)
                .put("snapshotId", snapshotId)
                .put("generation", snapshotGeneration)
                .put("nodes", nodes)
                .put("truncated", truncated)
                .put("source", "accessibilityService");
    }

    private JSONObject encodeNode(
            AccessibilityNodeInfo node,
            String backendId,
            String parentBackendId) throws JSONException {
        boolean sensitive = isSensitive(node);
        String text = safeString(node.getText());
        String description = safeString(node.getContentDescription());
        String hint = safeString(node.getHintText());
        String viewId = safeString(node.getViewIdResourceName());
        String className = safeString(node.getClassName());

        String name = !description.isEmpty()
                ? description
                : (!text.isEmpty() ? text : (!hint.isEmpty() ? hint : tailId(viewId)));
        String role = role(node, className);

        JSONArray states = new JSONArray();
        states.put(node.isEnabled() ? "enabled" : "disabled");
        if (node.isFocusable()) states.put("focusable");
        if (node.isFocused()) states.put("focused");
        if (node.isEditable()) states.put("editable");
        if (node.isScrollable()) states.put("scrollable");
        if (node.isChecked()) states.put("checked");
        if (node.isSelected()) states.put("selected");

        JSONArray actions = new JSONArray();
        if (node.isEnabled() && (node.isClickable() || node.isEditable())) {
            actions.put("click");
        }
        if (node.isEnabled() && node.isEditable() && !sensitive) {
            actions.put("type");
        }

        Rect bounds = new Rect();
        node.getBoundsInScreen(bounds);
        JSONObject boundsJson = new JSONObject()
                .put("x", bounds.left)
                .put("y", bounds.top)
                .put("width", Math.max(0, bounds.width()))
                .put("height", Math.max(0, bounds.height()));

        return new JSONObject()
                .put("backendId", backendId)
                .put("role", role)
                .put("name", sensitive ? "Sensitive field" : name)
                .put("description", sensitive ? "" : description)
                .put("text", sensitive || text.isEmpty() ? JSONObject.NULL : text)
                .put("value", sensitive || !node.isEditable() ? JSONObject.NULL : text)
                .put("states", states)
                .put("actions", actions)
                .put("bounds", boundsJson)
                .put("sensitive", sensitive)
                .put(
                        "parentBackendId",
                        parentBackendId == null ? JSONObject.NULL : parentBackendId)
                .put("childBackendIds", new JSONArray());
    }

    private JSONObject action(String backendId, String action, String text) throws JSONException {
        Target target;
        synchronized (snapshotLock) {
            String expectedPrefix =
                    currentSnapshotId == null || currentSnapshotId.length() < 2
                            ? ""
                            : "h:" + currentSnapshotId.substring(1) + ":";
            if (expectedPrefix.isEmpty()
                    || backendId == null
                    || !backendId.startsWith(expectedPrefix)) {
                return error("STALE_UI", "Semantic snapshot is no longer current.");
            }
            target = currentTargets.get(backendId);
        }

        if (target == null) {
            return error(
                    "UNKNOWN_REF",
                    "Semantic ref is not present in the current helper snapshot.");
        }

        AccessibilityNodeInfo node = resolve(target);
        if (node == null) {
            return error("STALE_UI", "Semantic target no longer exists.");
        }

        try {
            if (isPaymentSensitivePackage(node.getPackageName())) {
                blockPaymentApp();
                return paymentAppBlockedError();
            }

            if (!signature(node).equals(target.signature)) {
                return error("STALE_UI", "Semantic target changed before the action.");
            }

            if ("click".equals(action)) {
                if (!target.clickable && !target.editable) {
                    return error("NOT_CLICKABLE", "Semantic target is not clickable.");
                }

                boolean performed = node.performAction(AccessibilityNodeInfo.ACTION_CLICK);
                return new JSONObject()
                        .put("ok", performed)
                        .put("deviceAccepted", performed)
                        .put("verified", performed)
                        .put("reasonCode", performed ? JSONObject.NULL : "ACTION_REJECTED");
            }

            if ("type".equals(action) || "set_text".equals(action)) {
                if (target.sensitive || isSensitive(node)) {
                    return error(
                            "SENSITIVE_FIELD",
                            "RepoTunnel blocks semantic text entry into sensitive fields.");
                }
                if (!target.editable || !node.isEditable()) {
                    return error("NOT_EDITABLE", "Semantic target is not editable.");
                }
                if (text == null
                        || text.codePointCount(0, text.length()) < 1
                        || text.codePointCount(0, text.length()) > 2000) {
                    return error(
                            "INVALID_ARGUMENT",
                            "Text must contain 1..2000 characters.");
                }

                Bundle arguments = new Bundle();
                arguments.putCharSequence(
                        AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
                        text);
                boolean performed =
                        node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, arguments);
                if (!performed) {
                    return error("ACTION_REJECTED", "Android rejected ACTION_SET_TEXT.");
                }

                try {
                    Thread.sleep(40);
                } catch (InterruptedException interrupted) {
                    Thread.currentThread().interrupt();
                }

                AccessibilityNodeInfo after = resolve(target);
                boolean verified = false;
                if (after != null) {
                    try {
                        CharSequence value = after.getText();
                        verified = value != null && text.contentEquals(value);
                    } finally {
                        after.recycle();
                    }
                }

                return new JSONObject()
                        .put("ok", verified)
                        .put("deviceAccepted", true)
                        .put("verified", verified)
                        .put(
                                "reasonCode",
                                verified ? JSONObject.NULL : "TEXT_NOT_VERIFIED");
            }

            return error("INVALID_ARGUMENT", "Unsupported semantic action.");
        } finally {
            node.recycle();
        }
    }

    private AccessibilityNodeInfo resolve(Target target) {
        List<AccessibilityWindowInfo> windows = getWindows();
        AccessibilityNodeInfo current = null;

        if (windows != null) {
            for (AccessibilityWindowInfo window : windows) {
                if (window.getId() == target.windowId) {
                    current = window.getRoot();
                    break;
                }
            }
        }

        if (current == null) {
            AccessibilityNodeInfo root = getRootInActiveWindow();
            if (root != null && root.getWindowId() == target.windowId) {
                current = root;
            } else if (root != null) {
                root.recycle();
            }
        }

        if (current == null) {
            return null;
        }

        for (int index : target.path) {
            AccessibilityNodeInfo child =
                    index >= 0 && index < current.getChildCount()
                            ? current.getChild(index)
                            : null;
            current.recycle();
            if (child == null) {
                return null;
            }
            current = child;
        }

        return current;
    }

    private String signature(AccessibilityNodeInfo node) {
        Rect bounds = new Rect();
        node.getBoundsInScreen(bounds);
        String material =
                safeString(node.getPackageName())
                        + "|"
                        + safeString(node.getClassName())
                        + "|"
                        + safeString(node.getViewIdResourceName())
                        + "|"
                        + bounds.flattenToString()
                        + "|"
                        + safeString(node.getText())
                        + "|"
                        + safeString(node.getContentDescription())
                        + "|"
                        + node.isEnabled()
                        + "|"
                        + node.isClickable()
                        + "|"
                        + node.isEditable()
                        + "|"
                        + node.isPassword();
        return sha256(material);
    }

    private boolean isSensitive(AccessibilityNodeInfo node) {
        if (node.isPassword()) {
            return true;
        }

        int inputType = node.getInputType();
        int variation = inputType & InputType.TYPE_MASK_VARIATION;
        if (variation == InputType.TYPE_TEXT_VARIATION_PASSWORD
                || variation == InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
                || variation == InputType.TYPE_TEXT_VARIATION_WEB_PASSWORD
                || variation == InputType.TYPE_NUMBER_VARIATION_PASSWORD) {
            return true;
        }

        String joined =
                (safeString(node.getClassName())
                                + " "
                                + safeString(node.getViewIdResourceName())
                                + " "
                                + safeString(node.getContentDescription())
                                + " "
                                + safeString(node.getHintText()))
                        .toLowerCase(Locale.ROOT);
        String[] hints = {
            "password",
            "passwd",
            "passcode",
            "pin",
            "otp",
            "one-time",
            "verification code",
            "cvv",
            "cvc",
            "card number",
            "credit card",
            "debit card",
            "security code",
            "upi pin",
            "credential",
            "secret"
        };
        for (String hint : hints) {
            if (joined.contains(hint)) {
                return true;
            }
        }
        return false;
    }

    private String role(AccessibilityNodeInfo node, String className) {
        String simple = className;
        int dot = simple.lastIndexOf('.');
        if (dot >= 0) {
            simple = simple.substring(dot + 1);
        }

        if (node.isEditable()
                || "EditText".equals(simple)
                || "AutoCompleteTextView".equals(simple)
                || "MultiAutoCompleteTextView".equals(simple)) {
            return "textbox";
        }

        switch (simple) {
            case "Button":
            case "ImageButton":
                return "button";
            case "CheckBox":
                return "checkbox";
            case "RadioButton":
                return "radio";
            case "Switch":
            case "ToggleButton":
                return "switch";
            case "Spinner":
                return "combobox";
            case "SeekBar":
                return "slider";
            case "ImageView":
                return "image";
            case "ListView":
            case "RecyclerView":
                return "list";
            case "GridView":
                return "grid";
            case "WebView":
                return "webview";
            case "TextView":
                return "text";
            default:
                return "element";
        }
    }

    private String tailId(String viewId) {
        int slash = viewId.lastIndexOf('/');
        return slash >= 0 && slash + 1 < viewId.length()
                ? viewId.substring(slash + 1)
                : viewId;
    }

    private String safeString(CharSequence value) {
        return value == null ? "" : value.toString();
    }

    private String sha256(String value) {
        try {
            byte[] digest =
                    MessageDigest.getInstance("SHA-256")
                            .digest(value.getBytes(StandardCharsets.UTF_8));
            StringBuilder result = new StringBuilder(digest.length * 2);
            for (byte item : digest) {
                result.append(String.format(Locale.ROOT, "%02x", item & 0xff));
            }
            return result.toString();
        } catch (Exception ignored) {
            return Integer.toHexString(value.hashCode());
        }
    }

    private JSONObject error(String reasonCode, String message) {
        try {
            return new JSONObject()
                    .put("ok", false)
                    .put("reasonCode", reasonCode)
                    .put("message", message);
        } catch (JSONException impossible) {
            return new JSONObject();
        }
    }
}
