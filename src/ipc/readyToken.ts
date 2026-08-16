// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

export const READY_TOKEN_LENGTH = 32;
export const READY_TOKEN_PATTERN = /^[0-9a-f]{32}$/;
export const MAX_READY_TOKEN_SCAN_LENGTH = 4_194_304;

/**
 * A bounded deterministic fingerprint lets the UI compare a challenge without
 * retaining raw token history in Zustand, devtools, or error objects.
 */
export function fingerprintReadyToken(token: string): string {
  let left = 0x9e3779b1n;
  let right = 0xc2b2ae35n;
  const mask = 0xffffffffffffffffn;
  const normalized = token.toLowerCase();
  for (let index = 0; index < normalized.length; index += 1) {
    const code = BigInt(normalized.charCodeAt(index));
    left = ((left ^ code) * 0x100000001b3n) & mask;
    right = ((right + code + BigInt(index)) * 0x9e3779b185ebca87n) & mask;
    right ^= left >> 29n;
  }
  return `${left.toString(16).padStart(16, "0")}${right.toString(16).padStart(16, "0")}`;
}

/**
 * Scans bounded string windows so a token embedded in a diagnostic or object
 * key cannot bypass an equality-only leak check.
 */
export function forEachReadyTokenCandidate(value: string, visit: (candidate: string) => boolean): boolean {
  if (value.length > MAX_READY_TOKEN_SCAN_LENGTH) {
    throw new Error("string is too large for ready-token inspection");
  }
  if (value.length < READY_TOKEN_LENGTH) {
    return false;
  }
  for (let start = 0; start <= value.length - READY_TOKEN_LENGTH; start += 1) {
    const candidate = value.slice(start, start + READY_TOKEN_LENGTH);
    if (READY_TOKEN_PATTERN.test(candidate.toLowerCase()) && visit(candidate.toLowerCase())) {
      return true;
    }
  }
  return false;
}

/**
 * Error fields use shape-based redaction even when the current challenge is
 * unavailable, preventing token-shaped text from becoming a diagnostic leak.
 */
export function containsTokenShapedText(value: string): boolean {
  return forEachReadyTokenCandidate(value, () => true);
}
