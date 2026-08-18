// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

/**
 * Windows-only desktop smoke test for the real Tauri -> Rust -> Java JSONL
 * path.  The runner intentionally lives outside the product composition so
 * the debug-only launch seam remains testable without adding a test protocol.
 */

import { execFile, spawn } from "node:child_process";
import { Buffer } from "node:buffer";
import { mkdtemp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import process from "node:process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { chromium } from "@playwright/test";

const execFileAsync = promisify(execFile);
const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const java25Home = "C:\\Users\\24052\\.jdks\\liberica-25.0.2";
const java25 = join(java25Home, "bin", "java.exe");
const runDeadlineMs = 180_000;
const turnDeadlineMs = 30_000;
const closeDeadlineMs = 20_000;
const pollMs = 1_000;
const snapshotTimeoutMs = 5_000;
const incompleteObservationLimit = 64;
const frozenExitStageSequence = [
  "exit_requested_enter",
  "exit_requested_return",
  "exit_enter",
  "exit_return",
];
const frozenExitStages = new Set(frozenExitStageSequence);
const snapshotHelpers = new Set();
let snapshotTail = Promise.resolve();

/**
 * Creates one run-wide cancellation source; phase deadlines must not reset
 * because a stuck second launch otherwise outlives the runner's total budget.
 */
function createDeadline(label, durationMs) {
  const controller = new globalThis.AbortController();
  const deadline = Date.now() + durationMs;
  const timer = globalThis.setTimeout(() => {
    controller.abort(new Error(`${label} 超过 ${durationMs}ms`));
  }, durationMs);
  return {
    signal: controller.signal,
    deadline,
    cancel: () => globalThis.clearTimeout(timer),
  };
}

/**
 * Converts an AbortSignal reason into a normal Error so failed runs still
 * enter the existing summary writer instead of producing an unhandled abort.
 */
function throwIfAborted(signal) {
  if (signal?.aborted) {
    const reason = signal.reason;
    throw reason instanceof Error ? reason : new Error(String(reason ?? "E2E 已取消"));
  }
}

/**
 * Makes polling cancellable; cleanup can wake immediately without leaving a
 * timer or waiting for the next one-second process poll.
 */
function waitForDelay(durationMs, signal) {
  return new Promise((resolvePromise, rejectPromise) => {
    let timer;
    const onAbort = () => {
      globalThis.clearTimeout(timer);
      signal?.removeEventListener("abort", onAbort);
      rejectPromise(signal.reason instanceof Error ? signal.reason : new Error("E2E 已取消"));
    };
    if (signal?.aborted) {
      onAbort();
      return;
    }
    timer = globalThis.setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolvePromise();
    }, durationMs);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

/**
 * Bounds Playwright operations that have no native AbortSignal parameter;
 * product processes are still reaped by the phase cleanup after rejection.
 */
function raceWithSignal(operation, signal) {
  if (signal?.aborted) {
    return Promise.reject(signal.reason ?? new Error("E2E 已取消"));
  }
  return new Promise((resolvePromise, rejectPromise) => {
    const onAbort = () => rejectPromise(signal.reason ?? new Error("E2E 已取消"));
    signal?.addEventListener("abort", onAbort, { once: true });
    Promise.resolve()
      .then(operation)
      .then(
        (value) => {
          signal?.removeEventListener("abort", onAbort);
          resolvePromise(value);
        },
        (error) => {
          signal?.removeEventListener("abort", onAbort);
          rejectPromise(error);
        },
      );
  });
}

/**
 * Combines the run abort with a local short diagnostic budget when supported
 * by Node; the fallback keeps the local deadline bounded on older runtimes.
 */
function combineSignals(signals) {
  const active = signals.filter((signal) => signal !== undefined);
  if (active.length <= 1) {
    return active[0];
  }
  if (typeof globalThis.AbortSignal?.any === "function") {
    return globalThis.AbortSignal.any(active);
  }
  return active[active.length - 1];
}

/**
 * Keeps the test's temporary tree private to one invocation; the random
 * suffix also makes its workspace identity independent from a developer's
 * existing JA history database.
 */
async function createRunDirectories() {
  const root = await mkdtemp(join(tmpdir(), "ja-desktop-e2e-"));
  const workspace = join(root, "workspace");
  const settings = join(root, "settings");
  const webview = join(root, "webview");
  const runtime = join(root, "runtime");
  const appData = join(root, "appdata");
  const roaming = join(appData, "roaming");
  const local = join(appData, "local");
  try {
    await Promise.all([mkdir(workspace), mkdir(settings), mkdir(webview), mkdir(runtime), mkdir(roaming, { recursive: true }), mkdir(local, { recursive: true })]);
  } catch (error) {
    Object.defineProperty(error, "e2eRoot", { value: root, enumerable: false });
    throw error;
  }
  return { root, workspace, settings, webview, runtime, appData, roaming, local };
}

/**
 * Reserves an ephemeral TCP port instead of guessing from a static range,
 * which prevents an unrelated local Chromium instance from receiving CDP
 * traffic during a parallel developer run.
 */
async function reservePort() {
  const server = createServer();
  await new Promise((resolvePromise, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolvePromise);
  });
  const address = server.address();
  const port = typeof address === "object" && address !== null ? address.port : 0;
  await new Promise((resolvePromise) => server.close(resolvePromise));
  if (!Number.isInteger(port) || port < 1) {
    throw new Error("无法分配 CDP 端口");
  }
  return port;
}

/**
 * Writes only a keyless, loopback OpenAI-compatible profile.  No credential
 * reference is emitted because fake mode must never turn a secret-shaped
 * fixture into a provider success or persist secret material in plain JSON.
 */
async function writeSettings(settingsRoot) {
  const document = {
    schemaVersion: 1,
    revision: 1,
    theme: "dark",
    activeProfileRevision: "profile_e2e",
    profiles: [{
      profileRevision: "profile_e2e",
      name: "E2E Fake",
      provider: "openai",
      protocol: "openai_chat_completions",
      model: "ja-e2e-fake",
      baseUrl: "http://127.0.0.1:9/v1",
      supportsVision: false,
      accessMode: "workspace",
      skillRevisions: [],
      mcpRevisions: [],
    }],
    mcpServers: [],
    window: { width: 1280, height: 820, maximized: false },
  };
  await writeFile(join(settingsRoot, "settings.json"), `${JSON.stringify(document, null, 2)}\n`, "utf8");
  return document;
}

/**
 * Adds a fixed-size child-process snapshot so cleanup can prove ownership by
 * the exact spawned PID/parent chain rather than killing a name-matched user
 * process such as another Cargo build or WebView2 session.
 */
/**
 * Executes one CIM enumeration after the shared gate has admitted it; the
 * caller-level wrapper below owns queue release even when this operation fails.
 */
async function runProcessSnapshot(signal) {
  throwIfAborted(signal);
  const script = [
    "$ErrorActionPreference = 'Stop'",
    "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name,CommandLine,CreationDate | ConvertTo-Json -Compress",
  ].join("; ");
  const stdout = await new Promise((resolvePromise, rejectPromise) => {
    const child = execFile("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], {
      windowsHide: true,
      maxBuffer: 8 * 1024 * 1024,
      encoding: "utf8",
      timeout: snapshotTimeoutMs,
      killSignal: "SIGTERM",
      signal,
    }, (error, output) => {
      snapshotHelpers.delete(child.pid);
      if (error) {
        rejectPromise(signal?.aborted ? signal.reason ?? error : error);
        return;
      }
      resolvePromise(Buffer.isBuffer(output) ? output.toString("utf8") : String(output ?? ""));
    });
    if (child.pid) {
      snapshotHelpers.add(child.pid);
    }
  });
  if (stdout.trim() === "") {
    return [];
  }
  const value = JSON.parse(stdout);
  const entries = Array.isArray(value) ? value : [value];
  return entries
    .map((entry) => ({
      pid: Number(entry.ProcessId),
      parentPid: Number(entry.ParentProcessId),
      name: typeof entry.Name === "string" ? entry.Name : "",
      commandLine: typeof entry.CommandLine === "string" ? entry.CommandLine : "",
      creationDate: typeof entry.CreationDate === "string" ? entry.CreationDate : "",
    }))
    .filter((entry) => Number.isInteger(entry.pid) && entry.pid > 0);
}

/**
 * Serializes every process snapshot across watcher, UI assertions and cleanup;
 * rejected jobs are flattened so one timeout cannot poison future snapshots.
 */
async function processSnapshot(signal) {
  const queued = snapshotTail.then(() => runProcessSnapshot(signal));
  snapshotTail = queued.catch(() => undefined);
  return raceWithSignal(() => queued, signal);
}

/**
 * Waits briefly for the spawned command wrapper to appear with every identity
 * field present; cleanup never falls back to a PID-only root assumption.
 */
async function waitForRootIdentity(rootPid, deadline, signal) {
  while (Date.now() < deadline) {
    const entry = (await processSnapshot(signal)).find((candidate) => candidate.pid === rootPid);
    if (hasProcessIdentity(entry)) {
      return entry;
    }
    await waitForDelay(pollMs, signal);
  }
  throw new Error(`Tauri launcher root ${rootPid} 未取得完整进程身份`);
}

/**
 * Requires all identity fields used by cleanup; an incomplete CIM row is
 * observed as unsafe rather than being converted into a killable placeholder.
 */
function hasProcessIdentity(entry) {
  return Number.isInteger(entry?.pid) && entry.pid > 0
    && Number.isInteger(entry?.parentPid) && entry.parentPid >= 0
    && entry.name !== "" && entry.commandLine !== "" && entry.creationDate !== "";
}

/**
 * Keeps transient incomplete descendant rows bounded without weakening the
 * complete-identity requirement for the launcher root or any kill target.
 */
function createIncompleteObserved() {
  return { current: new Map(), history: [], dropped: 0 };
}

/**
 * Copies only the process fields needed to re-identify an incomplete CIM row;
 * missing values stay explicit so the runner never turns a partial row into a
 * PID-only cleanup candidate.
 */
function incompleteProcessMarker(entry) {
  return {
    pid: entry?.pid,
    parentPid: Number.isInteger(entry?.parentPid) ? entry.parentPid : null,
    name: typeof entry?.name === "string" ? entry.name : "",
    creationDate: typeof entry?.creationDate === "string" ? entry.creationDate : "",
    commandLine: typeof entry?.commandLine === "string" ? entry.commandLine : "",
  };
}

/**
 * Matches a partial row only when PID, process name and creation time all
 * remain known; absent fields are deliberately not treated as identity.
 */
function sameIncompleteIdentity(expected, actual) {
  return Number.isInteger(expected?.pid) && expected.pid > 0
    && Number.isInteger(actual?.pid) && actual.pid === expected.pid
    && expected.name !== "" && actual.name !== ""
    && expected.name.toLowerCase() === actual.name.toLowerCase()
    && expected.creationDate !== "" && actual.creationDate !== ""
    && expected.creationDate === actual.creationDate;
}

/**
 * Records one incomplete descendant with a bounded history; it is never added
 * to the killable observed map until a later complete snapshot upgrades it.
 */
function recordIncompleteObserved(incompleteObserved, entry) {
  if (!(incompleteObserved instanceof Object) || !Number.isInteger(entry?.pid) || entry.pid <= 0) {
    return;
  }
  const marker = incompleteProcessMarker(entry);
  const key = JSON.stringify([marker.pid, marker.name.toLowerCase(), marker.creationDate]);
  if (!incompleteObserved.current.has(key) && incompleteObserved.current.size >= incompleteObservationLimit) {
    incompleteObserved.dropped += 1;
    return;
  }
  incompleteObserved.current.set(key, marker);
  if (!incompleteObserved.history.some((item) => JSON.stringify([item.pid, item.name.toLowerCase(), item.creationDate]) === key)) {
    if (incompleteObserved.history.length >= incompleteObservationLimit) {
      incompleteObserved.dropped += 1;
    } else {
      incompleteObserved.history.push(marker);
    }
  }
}

/**
 * Reconciles prior partial rows before building the next owned closure: a
 * complete row is eligible for normal upgrade, a vanished row leaves bounded
 * history only, and a still-partial row remains explicitly unkillable.
 */
function reconcileIncompleteObserved(incompleteObserved, snapshot) {
  if (!(incompleteObserved instanceof Object)) {
    return;
  }
  for (const [key, marker] of incompleteObserved.current) {
    const candidate = snapshot.find((entry) => sameIncompleteIdentity(marker, entry));
    if (candidate === undefined || hasProcessIdentity(candidate)) {
      incompleteObserved.current.delete(key);
      continue;
    }
    incompleteObserved.current.set(key, incompleteProcessMarker(candidate));
  }
}

/**
 * Revalidates PID plus name, command line and creation time before any close
 * or kill operation, preventing PID reuse from crossing this run's boundary.
 */
function sameProcessIdentity(expected, actual) {
  return hasProcessIdentity(expected) && hasProcessIdentity(actual)
    && expected.pid === actual.pid
    && expected.name === actual.name
    && expected.commandLine === actual.commandLine
    && expected.creationDate === actual.creationDate;
}

/**
 * Parses only the two observed PowerShell CIM JSON forms (legacy /Date(ms)/
 * and ISO-8601 UTC/offset); unknown date text fails closed on PID reuse.
 */
function parseWindowsCreationDate(value) {
  const text = typeof value === "string" ? value : "";
  const legacyMatch = /^\/Date\((\d+)\)\/$/.exec(text);
  if (legacyMatch !== null) {
    const timestamp = Number(legacyMatch[1]);
    return Number.isSafeInteger(timestamp) ? timestamp : undefined;
  }
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,7})?(?:Z|[+-]\d{2}:\d{2})$/.test(text)) {
    return undefined;
  }
  const timestamp = Date.parse(text);
  return Number.isFinite(timestamp) ? timestamp : undefined;
}

/**
 * Rejects a parent-PID collision with a process created before this launcher;
 * Windows can reuse a parent PID, so exact fields alone must not authorize a
 * fallback kill of an older shared tool such as the Visual Studio helper.
 */
function isCreatedDuringRun(rootIdentity, candidate) {
  const rootTime = parseWindowsCreationDate(rootIdentity?.creationDate);
  const candidateTime = parseWindowsCreationDate(candidate?.creationDate);
  return rootTime !== undefined && candidateTime !== undefined && candidateTime >= rootTime;
}

/**
 * Locks the process-age boundary with invalid-root, invalid-candidate, older,
 * equal, and newer CIM fixtures before any real process can be launched.
 */
function assertCreationBoundaryContract() {
  const root = { creationDate: "2026-08-18T11:13:05.166Z" };
  if (isCreatedDuringRun({ creationDate: "not-a-date" }, root)) {
    throw new Error("creation boundary accepted an invalid root timestamp");
  }
  if (isCreatedDuringRun(root, { creationDate: "not-a-date" })) {
    throw new Error("creation boundary accepted an invalid candidate timestamp");
  }
  if (isCreatedDuringRun(root, { creationDate: "2026-08-18T11:13:05.165Z" })) {
    throw new Error("creation boundary accepted an older candidate");
  }
  if (!isCreatedDuringRun(root, { creationDate: root.creationDate })) {
    throw new Error("creation boundary rejected an equal timestamp");
  }
  if (!isCreatedDuringRun(root, { creationDate: "2026-08-18T11:13:05.167Z" })) {
    throw new Error("creation boundary rejected a newer candidate");
  }
  const offsetRoot = { creationDate: "2026-08-18T19:13:05.166000+08:00" };
  if (isCreatedDuringRun(offsetRoot, { creationDate: "2026-08-18T19:13:05.165000+08:00" })) {
    throw new Error("creation boundary accepted an older +08:00 candidate");
  }
  if (!isCreatedDuringRun(offsetRoot, { creationDate: offsetRoot.creationDate })) {
    throw new Error("creation boundary rejected an equal +08:00 timestamp");
  }
  if (!isCreatedDuringRun(offsetRoot, { creationDate: "2026-08-18T19:13:05.167000+08:00" })) {
    throw new Error("creation boundary rejected a newer +08:00 timestamp");
  }
  if (isCreatedDuringRun(offsetRoot, { creationDate: "2026-08-18T19:13:05.167000+0800" })) {
    throw new Error("creation boundary accepted a malformed offset");
  }
  const legacyRoot = { creationDate: "/Date(1786423603973)/" };
  if (isCreatedDuringRun(legacyRoot, { creationDate: "/Date(1786423603972)/" })) {
    throw new Error("creation boundary accepted an older legacy CIM timestamp");
  }
  if (!isCreatedDuringRun(legacyRoot, { creationDate: legacyRoot.creationDate })) {
    throw new Error("creation boundary rejected an equal legacy CIM timestamp");
  }
  if (!isCreatedDuringRun(legacyRoot, { creationDate: "/Date(1786423603974)/" })) {
    throw new Error("creation boundary rejected a newer legacy CIM timestamp");
  }
  if (isCreatedDuringRun(legacyRoot, { creationDate: "/Date(-1)/" })) {
    throw new Error("creation boundary accepted a negative legacy timestamp");
  }
}

/**
 * Computes a descendant closure only from a fully observed root identity;
 * incomplete descendants are evidence-only until a later snapshot completes
 * their identity, while missing roots still return no tree.
 */
function processTree(rootIdentity, snapshot, incompleteObserved) {
  reconcileIncompleteObserved(incompleteObserved, snapshot);
  const root = snapshot.find((entry) => sameProcessIdentity(rootIdentity, entry));
  if (root === undefined) {
    return undefined;
  }
  const owned = new Map([[root.pid, root]]);
  let changed = true;
  while (changed) {
    changed = false;
    for (const entry of snapshot) {
      if (!owned.has(entry.pid) && owned.has(entry.parentPid)) {
        if (!hasProcessIdentity(entry)) {
          recordIncompleteObserved(incompleteObserved, entry);
          continue;
        }
        if (!isCreatedDuringRun(rootIdentity, entry)) {
          continue;
        }
        owned.set(entry.pid, entry);
        changed = true;
      }
    }
  }
  return owned;
}

/**
 * Redacts temporary paths and token-like text before a summary can be kept;
 * the summary is intended as evidence, not as a raw process log.
 */
function redact(text, directories = {}) {
  let value = String(text ?? "");
  const paths = [repoRoot, java25Home, process.env.USERPROFILE, directories.root, directories.workspace, directories.settings, directories.webview, directories.runtime, directories.appData]
    .filter((path) => typeof path === "string" && path.length > 0)
    .sort((left, right) => right.length - left.length);
  for (const path of paths) {
    const segments = path.replace(/^\\\\\?\\/, "").split(/[\\/]+/).filter(Boolean);
    if (segments.length > 0) {
      const escaped = segments
        .map((segment) => segment.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
        .join("[\\\\/]+");
      value = value.replace(new RegExp(escaped, "gi"), "<e2e-path>");
    }
  }
  value = value.replace(/((?:api[_-]?key|token|secret|password)\s*[:=]\s*)("[^"]*"|'[^']*'|[^,\s&}]+)/gi, "$1<redacted>");
  return value.replace(/\r?\n/g, " ").slice(0, 500);
}

/**
 * Fails fast if Windows separator/case/extended-path redaction regresses;
 * this is an inline contract check so no second test framework is required.
 */
function assertSanitizerContract() {
  const fixtureRoot = "C:\\Users\\JaE2E\\Temp\\ja-desktop-e2e-Fixture";
  const variant = fixtureRoot.replaceAll("\\", "/").toUpperCase();
  const extended = `\\\\?\\${fixtureRoot}`;
  const output = redact(`a=${variant} b=${extended} token: "secret-value"`, { root: fixtureRoot });
  if (output.includes("JaE2E") || output.toLowerCase().includes("secret-value") || !output.includes("<e2e-path>")) {
    throw new Error("E2E sanitizer contract failed for Windows path/secret variants");
  }
}

/**
 * Serializes process-tree observation so a slow CIM query cannot overlap the
 * next poll and create an unbounded helper-process queue.  Stop awaits the
 * in-flight query before cleanup can inspect the final tree.
 */
function startProcessWatcher(rootIdentity, observed, incompleteObserved, signal) {
  let stopped = false;
  let wake = undefined;
  let failure;
  const completion = (async () => {
    while (!stopped) {
      try {
        const snapshot = await processSnapshot(signal);
        const tree = processTree(rootIdentity, snapshot, incompleteObserved);
        if (tree !== undefined) {
          for (const [pid, entry] of tree) {
            observed.set(pid, entry);
          }
        }
      } catch (error) {
        if (!stopped && !signal?.aborted && failure === undefined) {
          failure = error;
        }
        // A process can exit between CIM enumeration and the next poll.
      }
      if (stopped || signal?.aborted) {
        break;
      }
      try {
        await new Promise((resolvePromise, rejectPromise) => {
          let timer;
          let rejectNow;
          const finish = (callback, value) => {
            globalThis.clearTimeout(timer);
            signal?.removeEventListener("abort", rejectNow);
            callback(value);
          };
          const resolveNow = () => finish(resolvePromise);
          rejectNow = () => finish(rejectPromise, signal.reason ?? new Error("E2E 已取消"));
          timer = globalThis.setTimeout(resolveNow, pollMs);
          wake = resolveNow;
          signal?.addEventListener("abort", rejectNow, { once: true });
        });
      } catch {
        // The run deadline is the normal wake-up path for a stuck phase.
      } finally {
        wake = undefined;
      }
    }
  })();
  return {
    stop: async () => {
      stopped = true;
      wake?.();
      await completion;
    },
    get failure() {
      return failure;
    },
  };
}

/**
 * Reaps only still-tracked PowerShell observers from this invocation.  Normal
 * paths await each observer, but this guard keeps a forced test failure from
 * leaving a CIM helper while never treating the Node runner as a kill target.
 */
async function stopSnapshotHelpers(signal) {
  for (const pid of [...snapshotHelpers]) {
    try {
      await execFileAsync("taskkill.exe", ["/PID", String(pid), "/T", "/F"], {
        windowsHide: true,
        maxBuffer: 1 * 1024 * 1024,
        timeout: snapshotTimeoutMs,
        signal,
      });
    } catch {
      // The observer may have completed between the set lookup and taskkill.
    }
    if (signal?.aborted) {
      continue;
    }
    try {
      const current = await processSnapshot(signal);
      if (!current.some((entry) => entry.pid === pid)) {
        snapshotHelpers.delete(pid);
      }
    } catch {
      // Keep the PID registered when absence cannot be confirmed safely.
    }
  }
}

/**
 * Resolves Windows command shims before spawning so a Node child does not
 * depend on PowerShell's command resolution rules or an implicit shell path.
 */
async function locateCommand(command, signal) {
  throwIfAborted(signal);
  try {
    const { stdout } = await execFileAsync("where.exe", [command], {
      windowsHide: true,
      maxBuffer: 1 * 1024 * 1024,
      timeout: snapshotTimeoutMs,
      signal,
    });
    const resolved = stdout.split(/\r?\n/).map((line) => line.trim()).find(Boolean);
    if (resolved) {
      return resolved;
    }
  } catch {
    // The caller receives a stable missing-tool error when the fallback fails.
  }
  return command;
}

/**
 * Builds one explicit launch environment for the Tauri launcher.  The PATH
 * additions come only from resolved command shims; all unrelated host
 * variables remain unchanged and debug seams, including the phase exit trace,
 * stay inside this run's temporary runtime.
 */
function buildTauriEnv(directories, cdpPort, exitTracePath, rootProcessEnv, pnpmCommand, cargoCommand) {
  const additionalArgs = [
    `--remote-debugging-port=${cdpPort}`,
    `--user-data-dir=${directories.webview}`,
  ].join(" ");
  const env = {
    ...rootProcessEnv,
    APPDATA: directories.roaming,
    LOCALAPPDATA: directories.local,
    JA_E2E_RUNTIME_ROOT: directories.runtime,
    JA_E2E_SETTINGS_ROOT: directories.settings,
    JA_E2E_EXIT_TRACE_PATH: exitTracePath,
    VITE_JA_E2E_PROJECT_PATH: directories.workspace,
    JA_DEBUG_JAVA: java25,
    JA_DEBUG_JAR: join(repoRoot, "agent", "target", "ja.jar"),
    WEBVIEW2_USER_DATA_FOLDER: directories.webview,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: additionalArgs,
  };
  const inheritedPath = rootProcessEnv.PATH ?? rootProcessEnv.Path ?? "";
  env.PATH = [dirname(pnpmCommand), dirname(cargoCommand), inheritedPath]
    .filter((part) => part !== "")
    .join(";");
  if (Object.hasOwn(env, "Path")) {
    delete env.Path;
  }
  return env;
}

/**
 * Starts the existing Tauri dev command with only debug test seams.  WebView2
 * receives a unique user-data directory, CDP endpoint, and frozen phase trace
 * path through environment/Chromium arguments; no product manifest or
 * dependency changes are needed.
 */
function startTauri(directories, cdpPort, exitTracePath, rootProcessEnv, pnpmCommand, cargoCommand) {
  const env = buildTauriEnv(directories, cdpPort, exitTracePath, rootProcessEnv, pnpmCommand, cargoCommand);
  const child = spawn("pnpm.cmd", ["tauri:dev"], {
    env,
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
    // A .cmd shim needs a shell on Windows; the resolved pnpm/Cargo bins are
    // prepended only for this child while retaining the repository cwd.
    shell: true,
    cwd: repoRoot,
  });
  const output = { stdout: [], stderr: [] };
  const capture = (target, chunk) => {
    if (target.length < 400) {
      target.push(String(chunk));
    }
  };
  child.stdout?.on("data", (chunk) => capture(output.stdout, chunk));
  child.stderr?.on("data", (chunk) => capture(output.stderr, chunk));
  return { child, output };
}

/**
 * Returns a bounded launcher diagnostic so a setup panic fails the E2E
 * promptly instead of waiting for the full CDP deadline after the window is
 * already gone.
 */
function launcherDiagnostic(launch) {
  const stdout = launch.output.stdout.join("").slice(-1_500);
  const stderr = launch.output.stderr.join("").slice(-1_500);
  return `stdout=${stdout} stderr=${stderr}`;
}

/**
 * Keeps the final launcher diagnostics bounded and path-redacted so a failed
 * run records build/runtime clues without turning process output into a log
 * or secret transport.
 */
function launcherOutputSummary(launch, directories) {
  return {
    stdoutTail: redact(launch.output.stdout.join("").slice(-1_500), directories),
    stderrTail: redact(launch.output.stderr.join("").slice(-1_500), directories),
  };
}

/**
 * Accepts only an ordered prefix of the frozen Rust exit-trace contract;
 * partial shutdown evidence is retained, while paths, commands, secrets,
 * duplicates, and out-of-order lines are rejected before summary persistence.
 */
function parseExitTraceStages(text) {
  const rawLines = String(text ?? "").split(/\r?\n/);
  while (rawLines.at(-1) === "") {
    rawLines.pop();
  }
  const lines = rawLines;
  if (lines.length === 0) {
    return { status: "empty", stages: [] };
  }
  if (lines.length > frozenExitStageSequence.length) {
    return { status: "invalid", stages: [] };
  }
  const stages = [];
  for (const [index, line] of lines.entries()) {
    const stage = /^stage=([A-Za-z0-9._-]{1,96})$/.exec(line)?.[1];
    if (stage === undefined || !frozenExitStages.has(stage) || stage !== frozenExitStageSequence[index]) {
      return { status: "invalid", stages };
    }
    stages.push(stage);
  }
  return {
    status: stages.length === frozenExitStageSequence.length ? "complete" : "partial",
    stages,
  };
}

/**
 * Exercises strict trace parsing with hostile path, token, JSON, newline, and
 * whitespace inputs; this stays local so parser regressions fail before any
 * desktop process is launched and untrusted text never enters the summary.
 */
function assertExitTraceContract() {
  const accepted = [
    "stage=exit_requested_enter",
    "stage=exit_requested_return",
    "stage=exit_enter",
    "stage=exit_return",
  ].join("\n");
  const acceptedResult = parseExitTraceStages(`${accepted}\n`);
  if (acceptedResult.status !== "complete" || acceptedResult.stages.length !== frozenExitStageSequence.length) {
    throw new Error("exit trace contract rejected the frozen Rust stage sequence");
  }
  for (let length = 1; length < frozenExitStageSequence.length; length += 1) {
    const prefix = frozenExitStageSequence.slice(0, length).map((stage) => `stage=${stage}`).join("\n");
    const partial = parseExitTraceStages(`${prefix}\n`);
    if (partial.status !== "partial" || partial.stages.length !== length) {
      throw new Error("exit trace contract rejected a valid shutdown prefix");
    }
  }
  const rejected = [
    "stage=C:\\Users\\24052\\secret",
    "stage=api_key",
    '{"stage":"exit_enter"}',
    "stage=exit_enter\nstage=unexpected",
    "stage=exit_enter ",
    "stage=exit_requested_enter\nstage=exit_enter",
    "stage=exit_requested_enter\nstage=exit_requested_enter",
    "stage=exit_enter",
    "stage=run_returned",
  ];
  for (const fixture of rejected) {
    if (parseExitTraceStages(fixture).status !== "invalid") {
      throw new Error("exit trace contract accepted untrusted or non-canonical input");
    }
  }
}

/**
 * Reads the phase-specific trace only after force cleanup; missing or unreadable
 * files become stable diagnostic status while the caller has already reaped
 * the process tree and never exposes the fixed runtime path in evidence.
 */
async function readExitTrace(runId, phase, runtime) {
  const tracePath = join(runtime, `ja-exit-trace-${runId}-${phase}.jsonl`);
  try {
    return parseExitTraceStages(await readFile(tracePath, "utf8"));
  } catch {
    return { status: "missing", stages: [] };
  }
}

/**
 * Re-samples product identities with an independent short budget after the
 * graceful signal aborts; copying the pre-deadline map would report stale
 * processes as live and obscure the actual shutdown result.
 */
async function captureFreshLiveIdentities(observed) {
  const freshDeadline = createDeadline("graceful fresh snapshot", 3_000);
  try {
    const snapshot = await processSnapshot(freshDeadline.signal);
    return {
      live: [...observed.values()].filter((entry) => snapshot.some((candidate) => sameProcessIdentity(entry, candidate))),
      snapshotStatus: "fresh",
    };
  } catch (error) {
    return { live: [], snapshotStatus: "failed", snapshotError: error };
  } finally {
    freshDeadline.cancel();
  }
}

/**
 * Selects fresh evidence whenever the graceful wait was aborted; the pure
 * resolver keeps stale pre-abort identities out of `liveAfterGrace`.
 */
function resolveGracefulProductObservation(phase, productGrace, freshGrace) {
  const result = productGrace.aborted ? freshGrace : productGrace;
  const live = Array.isArray(result?.live) ? result.live : [];
  const snapshotError = result?.snapshotError ?? result?.error;
  const snapshotStatus = result?.snapshotStatus;
  let failure;
  if (snapshotStatus !== undefined && snapshotStatus !== "fresh") {
    // Keep the raw error for evidence, but expose only a stable message so
    // paths, command lines, and secrets cannot enter the failure summary.
    failure = new Error(`${phase} 产品进程 graceful cleanup 后 fresh snapshot 状态异常`);
  } else if (snapshotError !== undefined) {
    failure = new Error(`${phase} 产品进程 graceful cleanup 后无法完成 fresh snapshot`);
  } else if (live.length > 0) {
    failure = new Error(`${phase} 产品进程在 graceful deadline 后仍存活`);
  }
  return { live, failure, snapshotError };
}

/**
 * Routes an aborted graceful result through one independent fresh snapshot;
 * the normal result remains unchanged and does not incur another CIM query.
 */
async function settleGracefulProductObservation(phase, productObserved, productGrace) {
  const freshGrace = productGrace.aborted
    ? await captureFreshLiveIdentities(productObserved)
    : productGrace;
  return resolveGracefulProductObservation(phase, productGrace, freshGrace);
}

/**
 * Exercises the abort resolver without launching a process or sleeping; this
 * guards the stale-map regression while keeping the runner's contract local.
 */
function assertGracefulAbortContract() {
  const stale = [{ pid: 1 }];
  const resolved = resolveGracefulProductObservation("contract", { aborted: true, live: stale }, { live: [] });
  if (resolved.live.length !== 0 || resolved.failure !== undefined) {
    throw new Error("graceful abort contract retained stale product identities");
  }
  const failed = resolveGracefulProductObservation("contract", { aborted: true, live: stale }, {
    live: [],
    snapshotStatus: "failed",
    snapshotError: new Error("C:\\secret-token"),
  });
  if (failed.failure === undefined || failed.snapshotError?.message !== "C:\\secret-token") {
    throw new Error("graceful abort contract dropped fresh snapshot failure evidence");
  }
  if (failed.failure.message.includes("secret-token")) {
    throw new Error("graceful abort contract exposed raw snapshot error");
  }
}

/**
 * Validates the final native runtime target before launch so a malformed test
 * path cannot fall back to the developer's app-data directory.
 */
function assertLaunchRuntimeRoot(directories) {
  const root = resolve(directories.root);
  const runtime = resolve(directories.runtime);
  const comparable = (value) => String(value).replace(/^\\\\\?\\/, "").replace(/[\\/]+$/, "").toLowerCase();
  const rootValue = comparable(root);
  const runtimeValue = comparable(runtime);
  if (!runtimeValue.startsWith(`${rootValue}\\`) || runtimeValue === rootValue) {
    throw new Error("E2E runtime 目录必须是本轮临时 root 下的绝对路径");
  }
  return runtime;
}

/**
 * Decodes the debug sidecar's data-dir identity and proves it belongs to this
 * invocation before any UI assertion can pass against a developer database.
 */
function assertRuntimeIsolation(snapshot, directories) {
  const java = snapshot.find((entry) => entry.name.toLowerCase() === "java.exe"
    && entry.commandLine.includes("--data-dir-base64="));
  if (java === undefined) {
    throw new Error("未找到带隔离 data-dir 标识的 Java sidecar");
  }
  const encoded = /--data-dir-base64=([^\s"]+)/i.exec(java.commandLine)?.[1];
  if (!encoded) {
    throw new Error("Java sidecar 缺少 data-dir 标识");
  }
  const decoded = Buffer.from(encoded, "base64url").toString("utf8");
  const comparable = (value) => String(value).replace(/^\\\\\?\\/, "").replace(/[\\/]+$/, "").toLowerCase();
  const expectedRoot = comparable(directories.runtime);
  const actual = comparable(decoded);
  if (actual !== expectedRoot) {
    throw new Error("Java sidecar data-dir 未隔离到本轮临时 runtime 目录");
  }
  return {
    javaPid: java.pid,
    decodedDataDir: redact(decoded, directories),
    expectedRoot: redact(directories.runtime, directories),
  };
}

/**
 * Reads only metadata from the real app-data runtime so this run can prove
 * the debug seam prevented accidental writes without opening or changing it.
 */
async function captureRealRuntimeEvidence(baseEnv, directories) {
  const appData = baseEnv.APPDATA;
  const localAppData = baseEnv.LOCALAPPDATA;
  if (typeof appData !== "string" || appData.trim() === ""
    || typeof localAppData !== "string" || localAppData.trim() === "") {
    throw new Error("当前 Windows 环境缺少 APPDATA，无法建立真实 runtime 不变基线");
  }
  const roamingAppRoot = join(appData, "io.github.kongweiguang.ja");
  const localAppRoot = join(localAppData, "io.github.kongweiguang.ja");
  const candidates = {
    roamingAppRoot,
    roamingRuntime: join(roamingAppRoot, "runtime"),
    roamingSettings: join(roamingAppRoot, "settings"),
    localAppRoot,
    localWebView: join(localAppRoot, "webview"),
  };
  const describe = async (path) => {
    try {
      const metadata = await stat(path);
      return {
        exists: true,
        size: metadata.size,
        birthtimeMs: metadata.birthtimeMs,
        ctimeMs: metadata.ctimeMs,
        mtimeMs: metadata.mtimeMs,
      };
    } catch {
      return { exists: false };
    }
  };
  const entries = await Promise.all(Object.entries(candidates).map(async ([name, path]) => [name, {
    path: redact(path, directories),
    metadata: await describe(path),
  }]));
  return { candidates: Object.fromEntries(entries) };
}

/**
 * Fails the smoke run if the real user runtime changed while the isolated
 * sidecar was active; only metadata equality is required and no cleanup is
 * attempted against that user-owned directory.
 */
function assertRealRuntimeUnchanged(before, after) {
  if (JSON.stringify(before.candidates) !== JSON.stringify(after.candidates)) {
    throw new Error("真实 AppData runtime 在隔离 E2E 期间发生变化");
  }
}

/**
 * Waits for the WebView2 debugging endpoint instead of assuming that the
 * Tauri process being alive means the rendered page is ready.
 */
async function waitForCdp(cdpPort, deadline, launch, signal) {
  const endpoint = `http://127.0.0.1:${cdpPort}/json/version`;
  while (Date.now() < deadline) {
    throwIfAborted(signal);
    if (launch.child.exitCode !== null || launch.child.signalCode !== null) {
      throw new Error(`Tauri launcher exited before CDP: ${launcherDiagnostic(launch)}`);
    }
    try {
      const response = await globalThis.fetch(endpoint, { signal });
      if (response.ok) {
        return;
      }
    } catch {
      // The endpoint is expected to be absent while WebView2 initializes.
    }
    await waitForDelay(pollMs, signal);
  }
  throw new Error("WebView2 CDP 在期限内未启动");
}

/**
 * Locates the actual Tauri WebView page while ignoring devtools/blank targets;
 * page text is checked later so a connected CDP socket alone cannot pass.
 */
async function waitForPage(browser, deadline, signal) {
  while (Date.now() < deadline) {
    throwIfAborted(signal);
    for (const context of browser.contexts()) {
      for (const page of context.pages()) {
        const url = page.url();
        if (url.includes("localhost:1420") || url.includes("tauri://localhost")) {
          return page;
        }
      }
    }
    await waitForDelay(pollMs, signal);
  }
  throw new Error("未找到 JA Tauri WebView 页面");
}

/**
 * Captures only bounded browser diagnostics needed to classify an E2E fault;
 * paths and token-shaped values are redacted before they enter the summary.
 */
function attachPageDiagnostics(page, directories) {
  const diagnostics = { console: [], pageErrors: [], requestFailed: [] };
  const append = (target, value) => {
    if (target.length < 20) {
      target.push(redact(value, directories));
    }
  };
  page.on("console", (message) => append(diagnostics.console, `${message.type()}: ${message.text()}`));
  page.on("pageerror", (error) => append(diagnostics.pageErrors, error?.stack ?? error));
  page.on("requestfailed", (request) => append(diagnostics.requestFailed, `${request.method()} ${request.url()} ${request.failure()?.errorText ?? "failed"}`));
  return diagnostics;
}

/**
 * Installs a bounded probe before the React bundle registers Tauri callbacks.
 * The raw payload is summarized later, so this only observes the native event
 * boundary and never changes the product's event subscription or state model.
 */
async function installRawTauriEventProbe(page) {
  await page.addInitScript(() => {
    const install = () => {
      const internals = globalThis.__TAURI_INTERNALS__;
      if (internals === undefined || typeof internals.transformCallback !== "function") {
        return false;
      }
      if (internals.__jaE2eTransformCallbackWrapped === true) {
        return true;
      }
      const original = internals.transformCallback.bind(internals);
      internals.transformCallback = (callback, once) => {
        if (typeof callback !== "function") {
          return original(callback, once);
        }
        const wrapped = (value) => {
          try {
            const list = Array.isArray(globalThis.__JA_E2E_TAURI_EVENTS__)
              ? globalThis.__JA_E2E_TAURI_EVENTS__
              : [];
            if (list.length < 128) {
              list.push(value);
            }
            globalThis.__JA_E2E_TAURI_EVENTS__ = list;
          } catch {
            // Diagnostics must never interfere with a production callback.
          }
          return callback(value);
        };
        return original(wrapped, once);
      };
      internals.__jaE2eTransformCallbackWrapped = true;
      return true;
    };
    if (install()) {
      return;
    }
    const timer = globalThis.setInterval(() => {
      if (install()) {
        globalThis.clearInterval(timer);
      }
    }, 10);
    globalThis.setTimeout(() => globalThis.clearInterval(timer), 5_000);
  });
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.evaluate(async () => {
    const internals = globalThis.__TAURI_INTERNALS__;
    if (internals === undefined
      || typeof internals.transformCallback !== "function"
      || typeof internals.invoke !== "function") {
      return;
    }
    const handler = internals.transformCallback((value) => {
      const list = Array.isArray(globalThis.__JA_E2E_TAURI_EVENTS__)
        ? globalThis.__JA_E2E_TAURI_EVENTS__
        : [];
      if (list.length < 128) {
        list.push(value);
      }
      globalThis.__JA_E2E_TAURI_EVENTS__ = list;
    });
    globalThis.__JA_E2E_TAURI_LISTENER_ID__ = await internals.invoke("plugin:event|listen", {
      event: "ja://rpc/frame",
      target: { kind: "Any" },
      handler,
    });
  }).catch(() => undefined);
}

/**
 * Removes the diagnostic listener before the WebView closes so the probe does
 * not outlive its page or change later test phases.
 */
async function removeRawTauriEventProbe(page) {
  await page.evaluate(async () => {
    const listenerId = globalThis.__JA_E2E_TAURI_LISTENER_ID__;
    const internals = globalThis.__TAURI_INTERNALS__;
    if (listenerId === undefined || internals === undefined || typeof internals.invoke !== "function") {
      return;
    }
    await internals.invoke("plugin:event|unlisten", { event: "ja://rpc/frame", eventId: listenerId });
    globalThis.__JA_E2E_TAURI_LISTENER_ID__ = undefined;
  }).catch(() => undefined);
}

/**
 * Reduces native callback payloads to method/identity facts that distinguish
 * delivery from projection without persisting prompts, paths, or secrets.
 */
async function captureRawTauriEvents(page) {
  try {
    const values = await page.evaluate(() => (
      Array.isArray(globalThis.__JA_E2E_TAURI_EVENTS__) ? globalThis.__JA_E2E_TAURI_EVENTS__ : []
    ));
    return values.slice(-128).map((value) => {
      const envelope = value !== null && typeof value === "object" ? value : {};
      const payload = envelope.payload !== undefined ? envelope.payload : envelope;
      const root = payload !== null && typeof payload === "object" ? payload : {};
      const params = root.params !== null && typeof root.params === "object" ? root.params : {};
      const turn = params.turn !== null && typeof params.turn === "object" ? params.turn : {};
      const item = params.item !== null && typeof params.item === "object" ? params.item : {};
      return {
        event: typeof envelope.event === "string" ? envelope.event : undefined,
        method: typeof root.method === "string" ? root.method : undefined,
        threadId: typeof params.threadId === "string" ? params.threadId : undefined,
        seq: Number.isSafeInteger(params.seq) ? params.seq : undefined,
        eventId: typeof params.eventId === "string" ? params.eventId : undefined,
        serverInstanceId: typeof params.serverInstanceId === "string" ? params.serverInstanceId : undefined,
        turnId: typeof turn.turnId === "string" ? turn.turnId : typeof item.turnId === "string" ? item.turnId : undefined,
        itemId: typeof item.itemId === "string" ? item.itemId : typeof params.itemId === "string" ? params.itemId : undefined,
        status: typeof turn.status === "string" ? turn.status : typeof item.status === "string" ? item.status : typeof params.status === "string" ? params.status : undefined,
      };
    });
  } catch {
    return [];
  }
}

/**
 * Reads the visible composer/timeline state immediately after a send click;
 * these fields distinguish a rejected command from a missing event projection.
 */
async function captureUiEvidence(page, directories, parentSignal) {
  const captureDeadline = createDeadline("UI 诊断", 3_000);
  const signal = combineSignals([parentSignal, captureDeadline.signal]);
  const read = async (operation, fallback) => {
    try {
      return await raceWithSignal(operation, signal);
    } catch {
      return fallback;
    }
  };
  try {
    return {
      textareaValue: redact(await read(() => page.getByRole("textbox", { name: "消息" }).inputValue(), "<unavailable>"), directories),
      sendButtonCount: await read(() => page.getByRole("button", { name: "发送", exact: true }).count(), -1),
      cancelButtonCount: await read(() => page.getByRole("button", { name: "取消", exact: true }).count(), -1),
      alerts: (await read(() => page.getByRole("alert").allTextContents(), [])).map((value) => redact(value, directories)),
      timeline: redact(await read(() => page.getByRole("region", { name: "对话时间线" }).innerText(), "<unavailable>"), directories),
      tauriEvents: await read(() => captureRawTauriEvents(page), []),
    };
  } finally {
    captureDeadline.cancel();
  }
}

/**
 * Finds the compiled Tauri process among this run's descendants.  The name
 * check is only for selecting a window target; ownership still comes from the
 * recorded root descendant closure.
 */
function tauriProcessIds(tree, snapshot) {
  return [...tree.values()]
    .filter((entry) => (entry.name.toLowerCase() === "ja.exe" || /\\target\\(?:debug|release)\\ja\.exe/i.test(entry.commandLine))
      && snapshot.some((candidate) => sameProcessIdentity(entry, candidate)))
    .map((entry) => entry.pid);
}

/**
 * Posts WM_CLOSE to only the observed Tauri process windows so the app's own
 * ExitRequested cleanup runs before the runner applies its bounded fallback.
 */
async function requestWindowClose(processIds, signal) {
  throwIfAborted(signal);
  if (processIds.length === 0) {
    throw new Error("未找到可关闭的 Tauri 进程");
  }
  const pidList = processIds.join(",");
  const script = `Add-Type @'\nusing System;\nusing System.Runtime.InteropServices;\nusing System.Text;\npublic static class JaE2EWin32 {\n  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);\n  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr extra);\n  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);\n  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);\n  [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW", CharSet = CharSet.Unicode, SetLastError = true)] public static extern IntPtr GetWindowLongPtr(IntPtr hWnd, int index);\n  [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr hWnd, uint command);\n  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextLength(IntPtr hWnd);\n  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int maxCount);\n  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint message, IntPtr wParam, IntPtr lParam);\n}\n'@; $ids = @(${pidList}) | ForEach-Object { [uint32]$_ }; $WS_EX_TOOLWINDOW = 0x80; $windows = New-Object 'System.Collections.Generic.List[System.IntPtr]'; [JaE2EWin32]::EnumWindows({ param($hWnd, $extra); $owner = [uint32]0; [JaE2EWin32]::GetWindowThreadProcessId($hWnd, [ref]$owner) | Out-Null; if ($ids -notcontains $owner) { return $true }; if (-not [JaE2EWin32]::IsWindowVisible($hWnd)) { return $true }; $style = [JaE2EWin32]::GetWindowLongPtr($hWnd, -20).ToInt64(); if (($style -band $WS_EX_TOOLWINDOW) -ne 0) { return $true }; if ([JaE2EWin32]::GetWindow($hWnd, 4) -ne [IntPtr]::Zero) { return $true }; $titleLength = [JaE2EWin32]::GetWindowTextLength($hWnd); if ($titleLength -le 0) { return $true }; $title = New-Object System.Text.StringBuilder ($titleLength + 1); [JaE2EWin32]::GetWindowText($hWnd, $title, $title.Capacity) | Out-Null; if ([string]::IsNullOrWhiteSpace($title.ToString())) { return $true }; $windows.Add($hWnd); return $true }, [IntPtr]::Zero) | Out-Null; if ($windows.Count -ne 1) { throw "Tauri PID window selection expected exactly one main window, got $($windows.Count)" }; if (-not [JaE2EWin32]::PostMessage($windows[0], 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)) { throw "PostMessage WM_CLOSE failed" }`;
  await execFileAsync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], {
    windowsHide: true,
    maxBuffer: 1 * 1024 * 1024,
    timeout: snapshotTimeoutMs,
    signal,
  });
}

/**
 * Locks the native close seam to one visible, titled non-tool window; this
 * static guard prevents a future refactor from broadcasting WM_CLOSE to every
 * hidden Tao event target and losing the app's exit trace.
 */
function assertWindowCloseContract() {
  const source = requestWindowClose.toString();
  const uniqueSelection = source.indexOf("$windows.Count -ne 1");
  const postMessage = source.indexOf("[JaE2EWin32]::PostMessage", uniqueSelection);
  const required = [
    "IsWindowVisible",
    "GetWindowLongPtrW",
    "EntryPoint = \"GetWindowLongPtrW\"",
    "$WS_EX_TOOLWINDOW = 0x80",
    "GetWindowTextLength",
    "GetWindow",
    "$windows.Count -ne 1",
    "PostMessage WM_CLOSE failed",
  ];
  if (required.some((marker) => !source.includes(marker))
    || uniqueSelection < 0
    || postMessage < uniqueSelection
    || source.includes("if ($ids -contains $owner) { [JaE2EWin32]::PostMessage")) {
    throw new Error("Tauri main-window close contract failed");
  }
}

/**
 * Terminates only the exact process tree owned by this runner after the
 * graceful close deadline; every fallback PID was observed as a root
 * descendant, so unrelated Cargo/Java/WebView2 processes are untouched.
 */
async function stopProcessTree(rootIdentity, observed, signal) {
  const kill = async (pid, tree) => {
    try {
      await execFileAsync("taskkill.exe", ["/PID", String(pid), ...(tree ? ["/T"] : []), "/F"], {
        windowsHide: true,
        maxBuffer: 1 * 1024 * 1024,
        timeout: snapshotTimeoutMs,
        signal,
      });
      return true;
    } catch {
      // The process may have exited between the snapshot and taskkill.
      return false;
    }
  };
  if (signal?.aborted) {
    return;
  }
  if (!hasProcessIdentity(rootIdentity)) {
    return;
  }
  const current = await processSnapshot(signal);
  const currentRoot = current.find((entry) => sameProcessIdentity(rootIdentity, entry));
  if (currentRoot !== undefined) {
    // A single exact-root /T kill lets Windows traverse the already verified
    // tree; killing the stale descendant list serially can exhaust cleanup's
    // deadline while the dev wrapper is already gone.
    await kill(currentRoot.pid, true);
  }
  if (signal?.aborted) {
    return;
  }
  // Re-enumerate after the tree kill; only survivors with the original full
  // identity may receive a direct fallback kill, and those kills run in
  // parallel so one stuck Windows process cannot starve all cleanup.
  const afterRoot = await processSnapshot(signal);
  const survivors = [...observed.values()]
    .map((entry) => afterRoot.find((candidate) => sameProcessIdentity(entry, candidate)))
    .filter((entry) => entry !== undefined);
  if (survivors.length > 0) {
    await Promise.all(survivors.map((entry) => kill(entry.pid, false)));
  }
}

/**
 * Captures final identities with a new bounded signal; cleanup evidence must
 * never copy the pre-kill observed map when its main deadline has aborted.
 */
async function captureFreshTreeState(observed, rootIdentity, incompleteObserved) {
  const freshDeadline = createDeadline("cleanup fresh snapshot", snapshotTimeoutMs);
  try {
    const current = await processSnapshot(freshDeadline.signal);
    const tree = rootIdentity === undefined ? undefined : processTree(rootIdentity, current, incompleteObserved);
    if (tree !== undefined) {
      for (const [pid, entry] of tree) {
        observed.set(pid, entry);
      }
    }
    const live = [];
    const reused = [];
    for (const entry of observed.values()) {
      const candidate = current.find((item) => item.pid === entry.pid);
      if (candidate === undefined) {
        continue;
      }
      if (sameProcessIdentity(entry, candidate)) {
        live.push(candidate);
      } else {
        reused.push(candidate);
      }
    }
    return {
      live,
      reused,
      incompleteLive: [...(incompleteObserved?.current.values() ?? [])],
      aborted: false,
      snapshotStatus: "fresh",
    };
  } catch (error) {
    return {
      live: [],
      reused: [],
      incompleteLive: [],
      aborted: false,
      snapshotStatus: "failed",
      snapshotError: error,
    };
  } finally {
    freshDeadline.cancel();
  }
}

/**
 * Polls exact observed identities until no spawned descendant remains;
 * timeout/abort paths always replace stale state with a fresh bounded query.
 */
async function waitForTreeGone(observed, deadline, signal, rootIdentity, incompleteObserved) {
  while (Date.now() < deadline) {
    if (signal?.aborted) {
      const fresh = await captureFreshTreeState(observed, rootIdentity, incompleteObserved);
      return { ...fresh, aborted: true };
    }
    const current = await processSnapshot(signal);
    const tree = rootIdentity === undefined ? undefined : processTree(rootIdentity, current, incompleteObserved);
    if (tree !== undefined) {
      for (const [pid, entry] of tree) {
        observed.set(pid, entry);
      }
    }
    const live = [...observed.values()].filter((entry) => current.some((item) => sameProcessIdentity(entry, item)));
    const incompleteLive = [...(incompleteObserved?.current.values() ?? [])];
    if (live.length === 0 && incompleteLive.length === 0) {
      return { live: [], reused: [], incompleteLive: [], aborted: false, snapshotStatus: "fresh" };
    }
    await waitForDelay(pollMs, signal);
  }
  return captureFreshTreeState(observed, rootIdentity, incompleteObserved);
}

/**
 * Keeps command-line evidence useful for review while applying the same path
 * and secret sanitizer as all other user-visible diagnostics.
 */
function summarizeProcessIdentity(entry, directories) {
  return {
    pid: entry.pid,
    parentPid: entry.parentPid,
    name: entry.name,
    creationDate: entry.creationDate,
    commandLine: redact(entry.commandLine, directories),
  };
}

/**
 * Normalizes Windows path spelling for identity-scoped WebView2 selection and
 * sanitizer checks without treating unrelated same-name processes as owned.
 */
function windowsPathKey(value) {
  return String(value ?? "")
    .replace(/^\\\\\?\\/, "")
    .replace(/[\\/]+/g, "\\")
    .toLowerCase();
}

/**
 * Selects only the product processes already observed under this launcher;
 * dev wrappers remain outside this graceful gate and are force-reaped later.
 */
function productProcessEntries(observed, directories) {
  const webviewKey = windowsPathKey(directories.webview);
  return [...observed.values()].filter((entry) => {
    const name = entry.name.toLowerCase();
    const commandLine = entry.commandLine.toLowerCase();
    if (name === "ja.exe") {
      return true;
    }
    if (name === "java.exe") {
      return commandLine.includes("ja.jar")
        && commandLine.includes("--runtime=fake")
        && commandLine.includes("--data-dir-base64=");
    }
    return name === "msedgewebview2.exe"
      && commandLine.includes("--user-data-dir")
      && webviewKey !== ""
      && windowsPathKey(commandLine).includes(webviewKey);
  });
}

/**
 * Stops the watcher first, waits only for product identities during graceful
 * close, then force-reaps the remaining exact-identity dev tree separately.
 */
async function cleanupPhase(runId, phase, rootIdentity, observed, incompleteObserved, watcher, directories, evidence, runSignal) {
  let failure;
  let finalTree = { live: [], reused: [], incompleteLive: [], snapshotStatus: "not_captured" };
  let tauriPids = [];
  let productLiveAfterGrace = [];
  let liveAfterForce = [];
  let forcedDevTree = [];
  let productObserved = new Map();
  let gracefulSnapshot = { status: "not_captured" };
  let exitTrace;
  const graceful = createDeadline(`${phase} graceful cleanup`, closeDeadlineMs);
  try {
    await watcher?.stop();
    if (watcher?.failure !== undefined && !runSignal?.aborted) {
      failure = watcher.failure;
    }
    await stopSnapshotHelpers(graceful.signal);
    const current = await processSnapshot(graceful.signal);
    const tree = rootIdentity === undefined ? undefined : processTree(rootIdentity, current, incompleteObserved);
    if (tree !== undefined) {
      for (const [pid, entry] of tree) {
        observed.set(pid, entry);
      }
    }
    productObserved = new Map(productProcessEntries(observed, directories).map((entry) => [entry.pid, entry]));
    if (productObserved.size === 0) {
      failure ??= new Error(`${phase} 未观察到本轮产品进程身份`);
    }
    tauriPids = tauriProcessIds(observed, current);
    if (tauriPids.length > 0) {
      await requestWindowClose(tauriPids, graceful.signal);
    }
    try {
      const productGrace = await waitForTreeGone(productObserved, graceful.deadline, graceful.signal);
      const settledGrace = await settleGracefulProductObservation(phase, productObserved, productGrace);
      gracefulSnapshot = { status: settledGrace.snapshotStatus ?? "fresh", error: settledGrace.snapshotError };
      productLiveAfterGrace = settledGrace.live;
      failure ??= settledGrace.failure;
    } catch (error) {
      if (graceful.signal.aborted && productObserved.size > 0) {
        const settledGrace = await settleGracefulProductObservation(phase, productObserved, { aborted: true, live: [] });
        gracefulSnapshot = { status: settledGrace.snapshotStatus ?? "fresh", error: settledGrace.snapshotError };
        productLiveAfterGrace = settledGrace.live;
        failure ??= settledGrace.failure;
      } else if (!graceful.signal.aborted) {
        failure ??= error;
      }
    }
  } catch (error) {
    if (!graceful.signal.aborted) {
      failure ??= error;
    }
  } finally {
    graceful.cancel();
  }

  let force;
  const forcePreflight = createDeadline(`${phase} force preflight`, snapshotTimeoutMs);
  try {
    await stopSnapshotHelpers(forcePreflight.signal);
    const afterGrace = await processSnapshot(forcePreflight.signal);
    const afterGraceTree = rootIdentity === undefined ? undefined : processTree(rootIdentity, afterGrace, incompleteObserved);
    if (afterGraceTree !== undefined) {
      for (const [pid, entry] of afterGraceTree) {
        observed.set(pid, entry);
      }
    }
    const currentProduct = new Map(productProcessEntries(observed, directories).map((entry) => [entry.pid, entry]));
    liveAfterForce = [...currentProduct.values()].filter((entry) => afterGrace.some((candidate) => sameProcessIdentity(entry, candidate)));
    forcedDevTree = [...observed.values()].filter((entry) => afterGrace.some((candidate) => sameProcessIdentity(entry, candidate)));
    if (liveAfterForce.length > 0) {
      failure ??= new Error(`${phase} 产品进程在 graceful deadline 后仍存活`);
    }
    force = createDeadline(`${phase} force cleanup`, closeDeadlineMs);
    await stopProcessTree(rootIdentity, observed, force.signal);
    finalTree = await waitForTreeGone(observed, force.deadline, force.signal, rootIdentity, incompleteObserved);
    await stopSnapshotHelpers(force.signal);
    if (snapshotHelpers.size > 0) {
      failure ??= new Error("CIM snapshot helper force cleanup 后仍未确认退出");
    }
  } catch (error) {
    failure ??= error;
  } finally {
    forcePreflight.cancel();
    force?.cancel();
  }

  // A force signal can abort between taskkill and its final poll; obtain one
  // independent fresh identity set before writing evidence instead of copying
  // the historical observed map into liveAfterCleanup/liveAfterFinally.
  if (finalTree.snapshotStatus !== "fresh") {
    const freshFinalTree = await captureFreshTreeState(observed, rootIdentity, incompleteObserved);
    if (freshFinalTree.snapshotStatus === "fresh" || finalTree.snapshotStatus === "not_captured") {
      finalTree = freshFinalTree;
    }
  }
  if (finalTree.snapshotStatus !== "fresh") {
    failure ??= new Error(`${phase} 清理后无法取得 fresh process snapshot`);
  }

  evidence[phase].process = {
    root: rootIdentity === undefined ? undefined : summarizeProcessIdentity(rootIdentity, directories),
    observed: [...observed.values()].map((entry) => summarizeProcessIdentity(entry, directories)),
    tauriPids,
    liveAfterGrace: productLiveAfterGrace.map((entry) => summarizeProcessIdentity(entry, directories)),
    liveAfterForce: liveAfterForce.map((entry) => summarizeProcessIdentity(entry, directories)),
    forcedDevTree: forcedDevTree.map((entry) => summarizeProcessIdentity(entry, directories)),
    liveAfterCleanup: finalTree.live.map((entry) => summarizeProcessIdentity(entry, directories)),
    reusedPids: finalTree.reused.map((entry) => summarizeProcessIdentity(entry, directories)),
    cleanupSnapshot: {
      status: finalTree.snapshotStatus,
      error: finalTree.snapshotError === undefined ? undefined : redact(finalTree.snapshotError?.message ?? finalTree.snapshotError, directories),
    },
    gracefulSnapshot: {
      status: gracefulSnapshot.status,
      error: gracefulSnapshot.error === undefined ? undefined : redact(gracefulSnapshot.error?.message ?? gracefulSnapshot.error, directories),
    },
    incompleteObserved: {
      current: [...incompleteObserved.current.values()].map((entry) => summarizeProcessIdentity(entry, directories)),
      history: incompleteObserved.history.map((entry) => summarizeProcessIdentity(entry, directories)),
      dropped: incompleteObserved.dropped,
    },
  };
  exitTrace = await readExitTrace(runId, phase, directories.runtime);
  evidence[phase].exitTrace = {
    ...exitTrace,
    outcome: exitTrace.status === "complete" ? "complete" : "incomplete",
  };
  if (exitTrace.status !== "complete") {
    failure ??= new Error(`${phase} exit trace ${exitTrace.status}`);
  }
  evidence.tree.push({
    phase,
    rootPid: rootIdentity?.pid,
    liveAfterFinally: finalTree.live.map((entry) => entry.pid),
    liveAfterFinallyStatus: finalTree.snapshotStatus,
  });
  if (finalTree.live.length > 0) {
    failure ??= new Error(`${phase} 清理后仍有本轮进程`);
  }
  if (finalTree.incompleteLive?.length > 0) {
    failure ??= new Error(`${phase} 清理后仍有身份不完整的本轮进程，未执行 PID-only kill`);
  }
  return failure;
}

/**
 * Performs one complete UI turn and returns stable DOM evidence used by the
 * restart assertion.  The final answer is verified by text, not by a
 * screenshot or a client-side fake adapter.
 */
async function runFirstSession(page, runId, deadline, directories, recordAfterSend, recordIsolation, signal) {
  throwIfAborted(signal);
  await page.waitForLoadState("domcontentloaded");
  await page.getByRole("button", { name: "选择项目", exact: true }).waitFor({ state: "visible", timeout: Math.max(1, deadline - Date.now()) });
  await page.getByRole("button", { name: "选择项目", exact: true }).click();
  await page.getByRole("heading", { name: "开始 coding" }).waitFor({ state: "visible", timeout: Math.max(1, deadline - Date.now()) });
  await page.getByText("已连接", { exact: true }).waitFor({ state: "visible", timeout: Math.max(1, deadline - Date.now()) });
  await page.getByRole("list", { name: "历史对话列表" }).locator("[role=listitem]").first().waitFor({ state: "visible", timeout: Math.max(1, deadline - Date.now()) });
  recordIsolation(assertRuntimeIsolation(await processSnapshot(signal), directories));
  const input = `E2E turn ${runId}`;
  await page.getByRole("textbox", { name: "消息" }).fill(input);
  await page.getByRole("button", { name: "发送" }).click();
  recordAfterSend(await captureUiEvidence(page, directories, signal));
  throwIfAborted(signal);
  const turnDeadline = Math.min(deadline, Date.now() + turnDeadlineMs);
  // The fake response metadata echoes the prompt, so a broad hasText locator
  // matches both cards; the user item prefix is the stable protocol projection.
  await page.locator('.ja-chat-message[data-item-id^="item_user_"]').filter({ hasText: input }).waitFor({ state: "visible", timeout: Math.max(1, turnDeadline - Date.now()) });
  await page.getByText(`Fake response: ${input}`, { exact: true }).waitFor({ state: "visible", timeout: Math.max(1, turnDeadline - Date.now()) });
  await page.locator(".ja-chat-message").filter({ hasText: `Fake response: ${input}` }).waitFor({ state: "visible", timeout: Math.max(1, turnDeadline - Date.now()) });
  await page.getByText("工作过程", { exact: true }).waitFor({ state: "visible", timeout: Math.max(1, turnDeadline - Date.now()) }).catch(() => undefined);
  const timelineText = await page.getByRole("region", { name: "对话时间线" }).innerText();
  return { input, timelineText: redact(timelineText, directories) };
}

/**
 * Reconfigures the same workspace after a new Tauri process starts and proves
 * the previous durable user/fake rows came from SQLite snapshot restoration.
 */
async function runRestartSession(page, firstInput, deadline, directories, recordIsolation, signal) {
  throwIfAborted(signal);
  await page.waitForLoadState("domcontentloaded");
  await page.getByRole("button", { name: "选择项目", exact: true }).waitFor({ state: "visible", timeout: Math.max(1, deadline - Date.now()) });
  await page.getByRole("button", { name: "选择项目", exact: true }).click();
  await page.getByRole("heading", { name: "开始 coding" }).waitFor({ state: "visible", timeout: Math.max(1, deadline - Date.now()) });
  await page.getByText("已连接", { exact: true }).waitFor({ state: "visible", timeout: Math.max(1, deadline - Date.now()) });
  recordIsolation(assertRuntimeIsolation(await processSnapshot(signal), directories));
  await page.locator('.ja-chat-message[data-item-id^="item_user_"]').filter({ hasText: firstInput }).waitFor({ state: "visible", timeout: Math.max(1, deadline - Date.now()) });
  await page.getByText(`Fake response: ${firstInput}`, { exact: true }).waitFor({ state: "visible", timeout: Math.max(1, deadline - Date.now()) });
  return redact(await page.getByRole("region", { name: "对话时间线" }).innerText(), directories);
}

/**
 * Requires the independently built main jar so this smoke test cannot hide a
 * packaging failure by starting an implicit build process of its own.
 */
async function assertAgentJar(signal) {
  throwIfAborted(signal);
  const jar = join(repoRoot, "agent", "target", "ja.jar");
  const metadata = await stat(jar);
  if (!metadata.isFile() || metadata.size === 0) {
    throw new Error("agent/target/ja.jar 不存在或为空；请先由外层构建门生成");
  }
  return jar;
}

/**
 * Writes one bounded redacted artifact for both setup and runtime failures;
 * keeping this outside the lifecycle loop guarantees setup errors are visible.
 */
async function writeRunSummary(runId, evidence, status, error, directories) {
  const summaryRoot = join(tmpdir(), "ja-e2e-results");
  await mkdir(summaryRoot, { recursive: true });
  const summaryPath = join(summaryRoot, `${runId}.json`);
  const payload = {
    ...evidence,
    status,
    ...(error === undefined ? {} : { error: redact(error?.stack ?? error, directories) }),
  };
  await writeFile(summaryPath, `${JSON.stringify(payload, null, 2)}\n`, "utf8");
  return summaryPath;
}

/**
 * Removes only a runner-created temporary root; partial setup failures may
 * provide just this root, so the guard must not assume a complete directory object.
 */
async function cleanupRunRoot(root) {
  if (typeof root !== "string" || root.trim() === "") {
    return;
  }
  const resolvedRoot = resolve(root);
  const resolvedTemp = resolve(tmpdir()).replace(/[\\/]$/, "");
  const prefix = `${resolvedTemp}\\ja-desktop-e2e-`.toLowerCase();
  if (resolvedRoot.toLowerCase() === resolvedTemp.toLowerCase()
    || !resolvedRoot.toLowerCase().startsWith(prefix)) {
    return;
  }
  await rm(resolvedRoot, { recursive: true, force: true }).catch(() => undefined);
}

/**
 * Runs two real desktop lifecycles, keeping process-tree evidence separate
 * for each launch so a passing second session cannot hide a first-session
 * Cargo/Java/WebView2 leak.
 */
async function main() {
  if (process.platform !== "win32") {
    throw new Error("该 E2E 仅支持 Windows 11");
  }
  const runId = `run_${Date.now().toString(36)}`;
  assertSanitizerContract();
  assertCreationBoundaryContract();
  assertWindowCloseContract();
  assertGracefulAbortContract();
  assertExitTraceContract();
  const runDeadline = createDeadline("E2E 全局期限", runDeadlineMs);
  const baseEnv = { ...process.env };
  let directories = {};
  let cleanupRoot;
  let cdpPort;
  const evidence = {
    runId,
    cdpPort: undefined,
    setup: { directories: "pending", port: "pending", jar: "pending", settings: "pending" },
    realRuntime: {},
    first: {},
    second: {},
    tree: [],
  };
  let firstInput;
  try {
    throwIfAborted(runDeadline.signal);
    directories = await createRunDirectories();
    cleanupRoot = directories.root;
    evidence.setup.directories = "ready";
    cdpPort = await raceWithSignal(() => reservePort(), runDeadline.signal);
    evidence.cdpPort = cdpPort;
    evidence.setup.port = "ready";
    evidence.realRuntime.before = await captureRealRuntimeEvidence(baseEnv, directories);
    const jar = await assertAgentJar(runDeadline.signal);
    evidence.setup.jar = { present: true, path: redact(jar, directories) };
    const [pnpmCommand, cargoCommand] = await Promise.all([
      locateCommand("pnpm.cmd", runDeadline.signal),
      locateCommand("cargo.exe", runDeadline.signal),
    ]);
    await writeSettings(directories.settings);
    evidence.setup.settings = "ready";
    assertLaunchRuntimeRoot(directories);
    for (const phase of ["first", "second"]) {
      throwIfAborted(runDeadline.signal);
      assertLaunchRuntimeRoot(directories);
      const exitTracePath = join(directories.runtime, `ja-exit-trace-${runId}-${phase}.jsonl`);
      const launch = startTauri(directories, cdpPort, exitTracePath, baseEnv, pnpmCommand, cargoCommand);
      if (!launch.child.pid) {
        throw new Error(`${phase} Tauri launcher 没有 PID`);
      }
      const rootPid = launch.child.pid;
      const observed = new Map();
      const incompleteObserved = createIncompleteObserved();
      let rootIdentity;
      let watcher;
      let cleanupFailure;
      try {
        rootIdentity = await waitForRootIdentity(
          rootPid,
          Math.min(runDeadline.deadline, Date.now() + 10_000),
          runDeadline.signal,
        );
        observed.set(rootIdentity.pid, rootIdentity);
        const initial = await processSnapshot(runDeadline.signal);
        const initialTree = processTree(rootIdentity, initial, incompleteObserved);
        if (initialTree === undefined) {
          throw new Error(`${phase} 未能在初始快照中重验 launcher root`);
        }
        for (const [pid, entry] of initialTree) {
          observed.set(pid, entry);
        }
        watcher = startProcessWatcher(rootIdentity, observed, incompleteObserved, runDeadline.signal);
        const sessionDeadline = runDeadline.deadline;
        await waitForCdp(cdpPort, sessionDeadline, launch, runDeadline.signal);
        const browser = await raceWithSignal(
          () => chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`),
          runDeadline.signal,
        );
        let sessionError;
        let page;
        let diagnostics;
        try {
          page = await raceWithSignal(() => waitForPage(browser, sessionDeadline, runDeadline.signal), runDeadline.signal);
          await raceWithSignal(() => installRawTauriEventProbe(page), runDeadline.signal);
          diagnostics = attachPageDiagnostics(page, directories);
          if (phase === "first") {
            let afterSend;
            let isolation;
            const first = await raceWithSignal(() => runFirstSession(page, runId, sessionDeadline, directories, (value) => {
              afterSend = value;
            }, (value) => {
              isolation = value;
            }, runDeadline.signal), runDeadline.signal);
            firstInput = first.input;
            evidence.first = { timeline: first.timelineText, pageUrl: redact(page.url(), directories), pageTitle: redact(await page.title(), directories), afterSend, diagnostics, launcher: launcherOutputSummary(launch, directories), runtime: redact(directories.runtime, directories), isolation };
          } else {
            if (!firstInput) {
              throw new Error("重启断言缺少第一轮输入");
            }
            let isolation;
            const timeline = await raceWithSignal(() => runRestartSession(page, firstInput, sessionDeadline, directories, (value) => {
              isolation = value;
            }, runDeadline.signal), runDeadline.signal);
            evidence.second = { timeline, pageUrl: redact(page.url(), directories), pageTitle: redact(await page.title(), directories), diagnostics, launcher: launcherOutputSummary(launch, directories), runtime: redact(directories.runtime, directories), isolation };
          }
        } catch (error) {
          if (page !== undefined) {
            evidence[phase].afterFailure = await captureUiEvidence(page, directories, runDeadline.signal);
            if (phase === "first" && evidence[phase].afterSend === undefined) {
              evidence[phase].afterSend = evidence[phase].afterFailure;
            }
          }
          evidence[phase].diagnostics = diagnostics ?? { console: [], pageErrors: [], requestFailed: [], rawTauriEvents: [] };
          evidence[phase].launcher = launcherOutputSummary(launch, directories);
          sessionError = error;
          throw error;
        } finally {
          // Playwright's CDP Browser exposes close(), which only closes the
          // client transport for connectOverCDP; native WM_CLOSE remains the
          // sole operation that requests Tauri shutdown below.
          if (page !== undefined) {
            await raceWithSignal(() => removeRawTauriEventProbe(page), runDeadline.signal).catch(() => undefined);
          }
          try {
            await raceWithSignal(() => browser.close(), runDeadline.signal);
          } catch (closeError) {
            if (!sessionError) {
              sessionError = closeError;
            }
          }
        }
        if (sessionError !== undefined) {
          throw sessionError;
        }
      } finally {
        try {
          cleanupFailure = await cleanupPhase(runId, phase, rootIdentity, observed, incompleteObserved, watcher, directories, evidence, runDeadline.signal);
        } finally {
          // Capture the stream tail after WM_CLOSE/force cleanup so shutdown
          // markers emitted during process exit are retained in the summary.
          evidence[phase].launcher = launcherOutputSummary(launch, directories);
        }
      }
      if (cleanupFailure !== undefined) {
        throw cleanupFailure;
      }
    }
    if (!String(evidence.first.timeline).includes(`E2E turn ${runId}`)
      || !String(evidence.first.timeline).includes(`Fake response: E2E turn ${runId}`)
      || !String(evidence.second.timeline).includes(`E2E turn ${runId}`)
      || !String(evidence.second.timeline).includes(`Fake response: E2E turn ${runId}`)) {
      throw new Error("DOM 时间线缺少用户消息或 fake final");
    }
    evidence.realRuntime.after = await captureRealRuntimeEvidence(baseEnv, directories);
    assertRealRuntimeUnchanged(evidence.realRuntime.before, evidence.realRuntime.after);
    evidence.realRuntime.unchanged = true;
    const summaryPath = await writeRunSummary(runId, evidence, "passed", undefined, directories);
    process.stdout.write(`JA_E2E_OK run=${runId} cdp=${cdpPort} summary=${summaryPath}\n`);
  } catch (error) {
    if (directories.root === undefined && typeof error?.e2eRoot === "string") {
      directories = { root: error.e2eRoot };
      cleanupRoot = error.e2eRoot;
    }
    let reportError = error;
    if (evidence.realRuntime.before !== undefined) {
      try {
        evidence.realRuntime.after = await captureRealRuntimeEvidence(baseEnv, directories);
        assertRealRuntimeUnchanged(evidence.realRuntime.before, evidence.realRuntime.after);
        evidence.realRuntime.unchanged = true;
      } catch (runtimeError) {
        reportError = new Error(`${redact(error?.stack ?? error, directories)}; ${redact(runtimeError?.stack ?? runtimeError, directories)}`);
      }
    }
    const summaryPath = await writeRunSummary(runId, evidence, "failed", reportError, directories);
    process.stderr.write(`JA_E2E_FAILED run=${runId} summary=${summaryPath}\n`);
    throw reportError;
  } finally {
    runDeadline.cancel();
    await cleanupRunRoot(cleanupRoot ?? directories.root);
  }
}

await main();
