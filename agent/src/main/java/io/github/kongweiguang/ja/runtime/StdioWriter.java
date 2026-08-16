// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.runtime;

import io.github.kongweiguang.ja.protocol.HandshakeJsonlCodec;
import io.github.kongweiguang.ja.protocol.JaErrorCode;
import io.github.kongweiguang.ja.protocol.ProtocolException;
import io.github.kongweiguang.ja.protocol.ProtocolLimits;
import io.github.kongweiguang.ja.protocol.RpcEnvelope;

import java.io.IOException;
import java.io.OutputStream;
import java.util.Objects;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * Owns the only stdout writer for one sidecar generation.
 *
 * Encoding before enqueueing and flushing after every frame make the queue the
 * serialization boundary: handlers can run concurrently without interleaving
 * bytes or allowing diagnostic output to become a protocol frame.
 */
public final class StdioWriter implements AutoCloseable {
    /** Receives a local failure without writing a diagnostic to stdout. */
    @FunctionalInterface
    public interface FailureHandler {
        void onFailure(IOException exception);
    }

    private static final byte[] POISON = new byte[0];

    private final OutputStream output;
    private final HandshakeJsonlCodec codec;
    private final ProtocolLimits limits;
    private final ArrayBlockingQueue<byte[]> queue;
    private final FailureHandler failureHandler;
    private final AtomicBoolean accepting = new AtomicBoolean(true);
    private final AtomicBoolean closeRequested = new AtomicBoolean(false);
    private final Thread worker;

    /** Starts a virtual writer so a slow pipe cannot block request handlers. */
    public StdioWriter(OutputStream output, HandshakeJsonlCodec codec,
                       ProtocolLimits limits, FailureHandler failureHandler) {
        this.output = Objects.requireNonNull(output, "output");
        this.codec = Objects.requireNonNull(codec, "codec");
        this.limits = Objects.requireNonNull(limits, "limits");
        this.queue = new ArrayBlockingQueue<>(limits.maxOutboundQueueFrames());
        this.failureHandler = Objects.requireNonNull(failureHandler, "failureHandler");
        this.worker = Thread.ofVirtual().name("ja-stdio-writer").start(this::writeLoop);
    }

    /**
     * Encodes and queues one envelope; bounded admission prevents a fast
     * provider from retaining unbounded output while the host is stalled.
     */
    public synchronized void send(RpcEnvelope envelope) {
        Objects.requireNonNull(envelope, "envelope");
        if (!accepting.get()) {
            throw new ProtocolException(JaErrorCode.SHUTTING_DOWN);
        }
        byte[] frame = codec.encode(envelope, limits);
        if (!queue.offer(frame)) {
            throw new ProtocolException(JaErrorCode.QUEUE_FULL);
        }
    }

    /** Reports whether the writer can still accept a frame from a producer. */
    public boolean accepting() {
        return accepting.get() && !closeRequested.get();
    }

    /** Returns the number of encoded frames waiting for the single writer. */
    public int queuedFrames() {
        return queue.size();
    }

    /**
     * Stops accepting new frames but drains already accepted frames before
     * closing stdout, which keeps shutdown responses observable by the host.
     */
    public synchronized void requestClose() {
        if (!closeRequested.compareAndSet(false, true)) {
            return;
        }
        accepting.set(false);
        try {
            if (!queue.offer(POISON, 2, TimeUnit.SECONDS)) {
                worker.interrupt();
            }
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
        }
    }

    /** Waits a bounded time so a broken pipe cannot keep the sidecar alive. */
    @Override
    public void close() {
        requestClose();
        if (Thread.currentThread() == worker) {
            return;
        }
        try {
            worker.join(2_000);
            if (worker.isAlive()) {
                // A broken pipe must not keep the sidecar alive forever; the
                // producer queue has already been closed, so interruption
                // cannot discard a frame that was still contractually pending.
                worker.interrupt();
            }
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
        }
    }

    /** Writes frames in queue order and never lets handler threads touch stdout. */
    private void writeLoop() {
        try {
            while (true) {
                byte[] frame = queue.take();
                if (frame == POISON) {
                    return;
                }
                output.write(frame);
                output.flush();
            }
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
        } catch (IOException exception) {
            accepting.set(false);
            failureHandler.onFailure(exception);
        }
    }
}
