// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { JA_ERROR_CODES, JaError } from "./errors";
import { assertNoReadyTokenLeak, ReadyTokenSchema } from "./protocol";
import { fingerprintReadyToken } from "./readyToken";

export type HandshakePhase =
  | "disconnected"
  | "awaiting_initialized"
  | "awaiting_ready"
  | "ready"
  | "reconnect_required";

export interface HandshakeFailure {
  code: typeof JA_ERROR_CODES.HANDSHAKE_FAILED;
  jaCode: "HANDSHAKE_FAILED";
  retryable: false;
}

/**
 * This public projection is intentionally small: comparison fingerprints and
 * their history stay in a module-private WeakMap instead of Zustand/devtools.
 */
export interface HandshakeProjection {
  phase: HandshakePhase;
  generation: number;
  error?: HandshakeFailure;
}

export type HandshakeSignal =
  | { kind: "initialized"; readyToken: string }
  | { kind: "runtime"; status: "starting" | "ready" | "degraded" | "shutting_down" | "stopped" | "crashed"; readyToken?: string };
export type RuntimeStatus = Extract<HandshakeSignal, { kind: "runtime" }>["status"];

const MAX_TOKEN_HISTORY = 64;

interface HandshakeMetadata {
  expectedFingerprint?: string;
  usedFingerprints: readonly string[];
}

const metadataByProjection = new WeakMap<HandshakeProjection, HandshakeMetadata>();

function copyProjection(value: HandshakeProjection): HandshakeProjection {
  const copy: HandshakeProjection = {
    phase: value.phase,
    generation: value.generation,
    ...(value.error === undefined ? {} : { error: Object.freeze({ ...value.error }) }),
  };
  return Object.freeze(copy);
}

function metadataFor(state: HandshakeProjection): HandshakeMetadata {
  return metadataByProjection.get(state) ?? { usedFingerprints: [] };
}

function installProjection(
  value: HandshakeProjection,
  metadata: HandshakeMetadata,
): HandshakeProjection {
  const projection = copyProjection(value);
  metadataByProjection.set(projection, {
    expectedFingerprint: metadata.expectedFingerprint,
    usedFingerprints: Object.freeze([...metadata.usedFingerprints]),
  });
  return projection;
}

/**
 * Creates the state shown to UI consumers; the explicit non-ready phase
 * prevents a snapshot or business event from being mistaken as ready.
 */
export function createHandshakeProjection(): HandshakeProjection {
  return installProjection(
    { phase: "awaiting_initialized", generation: 0 },
    { usedFingerprints: [] },
  );
}

function withFailureHistory(state: HandshakeProjection): HandshakeProjection {
  return installProjection(
    {
      phase: "reconnect_required",
      generation: state.generation,
      error: {
        code: JA_ERROR_CODES.HANDSHAKE_FAILED,
        jaCode: "HANDSHAKE_FAILED",
        retryable: false,
      },
    },
    metadataFor(state),
  );
}

/** Keeps the bounded opaque history when a new challenge is admitted. */
function nextHistory(state: HandshakeProjection, fingerprint: string): readonly string[] {
  return [...metadataFor(state).usedFingerprints, fingerprint].slice(-MAX_TOKEN_HISTORY);
}

/**
 * Applies one handshake signal as a finite-state transition; invalid order,
 * missing challenge, duplicate ready, and old-generation values fail closed.
 */
function transitionHandshake(state: HandshakeProjection, signal: HandshakeSignal): HandshakeProjection {
  if (state.phase === "disconnected" || state.phase === "reconnect_required") {
    return withFailureHistory(state);
  }
  if (signal.kind === "initialized") {
    if (!ReadyTokenSchema.safeParse(signal.readyToken).success) {
      return withFailureHistory(state);
    }
    const fingerprint = fingerprintReadyToken(signal.readyToken);
    const metadata = metadataFor(state);
    if (state.phase !== "awaiting_initialized" || metadata.usedFingerprints.includes(fingerprint)) {
      return withFailureHistory(state);
    }
    return installProjection(
      {
        phase: "awaiting_ready",
        generation: state.generation + 1,
      },
      {
        expectedFingerprint: fingerprint,
        usedFingerprints: nextHistory(state, fingerprint),
      },
    );
  }

  if (signal.status === "ready") {
    const metadata = metadataFor(state);
    if (
      state.phase !== "awaiting_ready" ||
      signal.readyToken === undefined ||
      !ReadyTokenSchema.safeParse(signal.readyToken).success ||
      metadata.expectedFingerprint !== fingerprintReadyToken(signal.readyToken)
    ) {
      return withFailureHistory(state);
    }
    return installProjection({ phase: "ready", generation: state.generation }, metadata);
  }

  if (signal.readyToken !== undefined) {
    return withFailureHistory(state);
  }
  if (signal.status === "stopped" || signal.status === "crashed") {
    if (state.phase === "awaiting_ready") {
      return withFailureHistory(state);
    }
    const metadata = metadataFor(state);
    return installProjection(
      { phase: "awaiting_initialized", generation: state.generation },
      { usedFingerprints: metadata.usedFingerprints },
    );
  }
  return state;
}

/**
 * Builds the one stable error shared by client faults and reducer state;
 * challenge values are absent from both message and details.
 */
export function handshakeFailedError(): JaError {
  return new JaError("JA handshake failed; reconnect is required", {
    code: JA_ERROR_CODES.HANDSHAKE_FAILED,
    jaCode: "HANDSHAKE_FAILED",
    retryable: false,
  });
}

/**
 * The client keeps only the current opaque fingerprint; all historical
 * comparison state is bounded opaque fingerprints in the WeakMap.
 */
export class ReadyHandshake {
  #state: HandshakeProjection = createHandshakeProjection();
  #expectedFingerprint: string | undefined;
  #listeners = new Set<(state: HandshakeProjection, opaqueFingerprints: readonly string[]) => void>();

  get state(): HandshakeProjection {
    return copyProjection(this.#state);
  }

  get isReady(): boolean {
    return this.#state.phase === "ready";
  }

  /** Starts a fresh transport generation while retaining bounded history. */
  start(): void {
    const metadata = metadataFor(this.#state);
    this.#expectedFingerprint = undefined;
    this.#state = installProjection(
      { phase: "awaiting_initialized", generation: this.#state.generation },
      { usedFingerprints: metadata.usedFingerprints },
    );
    this.emit();
  }

  /** Marks the transport disconnected without making stale state look ready. */
  disconnect(): void {
    const metadata = metadataFor(this.#state);
    this.#expectedFingerprint = undefined;
    this.#state = installProjection(
      { phase: "disconnected", generation: this.#state.generation },
      { usedFingerprints: metadata.usedFingerprints },
    );
    this.emit();
  }

  /** Accepts only the client-owned outbound challenge once per generation. */
  acceptInitialized(token: string): HandshakeProjection {
    if (!ReadyTokenSchema.safeParse(token).success) {
      this.fail();
      return this.#state;
    }
    const next = transitionHandshake(this.#state, { kind: "initialized", readyToken: token });
    if (next.phase !== "awaiting_ready") {
      this.fail();
      return this.#state;
    }
    this.#expectedFingerprint = fingerprintReadyToken(token);
    this.#state = next;
    this.emit();
    return this.#state;
  }

  /** Verifies exact token echo and status ordering before exposing ready. */
  acceptRuntimeStatus(status: RuntimeStatus, token?: string): HandshakeProjection {
    if (
      status === "ready" &&
      (token === undefined || !ReadyTokenSchema.safeParse(token).success || fingerprintReadyToken(token) !== this.#expectedFingerprint)
    ) {
      this.fail();
      return this.#state;
    }
    const next = transitionHandshake(this.#state, { kind: "runtime", status, readyToken: token });
    if (next.phase === "reconnect_required") {
      this.fail();
      return this.#state;
    }
    this.#state = next;
    if (status === "stopped" || status === "crashed") {
      this.#expectedFingerprint = undefined;
    }
    this.emit();
    return this.#state;
  }

  /**
   * Runs the whole-frame redaction check before parsers or listeners see a
   * frame; history is compared by fingerprint without retaining old tokens.
   */
  assertFrameSafe(value: unknown): void {
    const root = value !== null && typeof value === "object" ? value as Record<string, unknown> : undefined;
    const params = root?.["params"];
    const paramsObject = params !== null && typeof params === "object" ? params as Record<string, unknown> : undefined;
    const allowChallengePath =
      (root?.["method"] === "initialized" ||
        (root?.["method"] === "runtime/statusChanged" && paramsObject?.["status"] === "ready"))
        ? ["params", "readyToken"] as const
        : undefined;
    const usedFingerprints = metadataFor(this.#state).usedFingerprints;
    assertNoReadyTokenLeak(value, {
      allowChallengePath,
      isKnownReadyToken: (candidate) => {
        if (!ReadyTokenSchema.safeParse(candidate).success) {
          return false;
        }
        return usedFingerprints.includes(fingerprintReadyToken(candidate));
      },
    });
  }

  /** Puts the projection into the stable error state without leaking a token. */
  failHandshake(): void {
    this.fail();
  }

  /** Subscribes UI boot/reconnect state without exposing token material. */
  onChange(listener: (state: HandshakeProjection, opaqueFingerprints: readonly string[]) => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  private fail(): void {
    this.#expectedFingerprint = undefined;
    this.#state = withFailureHistory(this.#state);
    this.emit();
  }

  private emit(): void {
    const opaqueFingerprints = Object.freeze([...metadataFor(this.#state).usedFingerprints]);
    for (const listener of this.#listeners) {
      try {
        listener(copyProjection(this.#state), opaqueFingerprints);
      } catch {
        // A UI observer must not prevent the transport state from reaching a safe phase.
      }
    }
  }
}
