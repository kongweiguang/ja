// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;

import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Objects;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Idempotent cancellation primitive. Listener registration and draining share
 * one lock so every listener observes cancellation at most once.
 */
public final class CancellationToken {
    /** Bounds callbacks retained by one operation, including post-cancel registrations. */
    public static final int DEFAULT_MAX_LISTENERS = 1_024;
    /** Absolute callback cap prevents a single turn from pinning arbitrary listener memory. */
    public static final int MAX_LISTENERS = 65_536;

    private final AtomicBoolean cancelled = new AtomicBoolean();
    private final int maxListeners;
    private final Object monitor = new Object();
    /** false means pending, true means the identity has already been invoked. */
    private final IdentityHashMap<Runnable, Boolean> listeners = new IdentityHashMap<>();

    /** Creates a token with the bounded default listener budget. */
    public CancellationToken() {
        this(DEFAULT_MAX_LISTENERS);
    }

    /** Creates a token with an explicit listener cap so misuse fails closed. */
    public CancellationToken(int maxListeners) {
        if (maxListeners < 1 || maxListeners > MAX_LISTENERS) {
            throw new IllegalArgumentException("maxListeners is outside absolute bounds");
        }
        this.maxListeners = maxListeners;
    }

    /** Returns whether cancellation has already been published. */
    public boolean isCancelled() {
        return cancelled.get();
    }

    /**
     * Registers a listener once; a listener registered after cancellation runs
     * immediately so no race can leave an operation uninterruptible.
     */
    public void onCancel(Runnable listener) {
        Objects.requireNonNull(listener, "listener");
        boolean runNow;
        synchronized (monitor) {
            if (listeners.containsKey(listener)) {
                return;
            }
            if (listeners.size() >= maxListeners) {
                throw new ProtocolException(JaErrorCode.QUEUE_FULL);
            }
            runNow = cancelled.get();
            listeners.put(listener, runNow);
        }
        if (runNow) {
            listener.run();
        }
    }

    /** Publishes cancellation once and invokes each distinct listener once. */
    public boolean cancel() {
        List<Runnable> callbacks;
        synchronized (monitor) {
            if (cancelled.get()) {
                return false;
            }
            cancelled.set(true);
            callbacks = new ArrayList<>();
            for (var entry : listeners.entrySet()) {
                if (!entry.getValue()) {
                    entry.setValue(true);
                    callbacks.add(entry.getKey());
                }
            }
        }
        RuntimeException firstFailure = null;
        for (Runnable callback : callbacks) {
            try {
                callback.run();
            } catch (RuntimeException failure) {
                if (firstFailure == null) {
                    firstFailure = failure;
                }
            }
        }
        if (firstFailure != null) {
            throw firstFailure;
        }
        return true;
    }
}
