// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import type { ProjectPicker } from "./useJaSession";

/**
 * Describes only the Vite values needed by the development-only E2E seam so
 * the production composition cannot accidentally depend on a test runner.
 */
export interface JaE2eEnvironment {
  readonly DEV: boolean;
  readonly VITE_JA_E2E_PROJECT_PATH?: string;
}

const WINDOWS_DRIVE_ABSOLUTE = /^[A-Za-z]:[\\/](?:.*)?$/;
const WINDOWS_UNC_ABSOLUTE = /^\\\\[^\\/]+[\\/][^\\/]+(?:[\\/].*)?$/;
const UNIX_ABSOLUTE = /^\/(?:.*)?$/;

/**
 * Accepts only paths that can be handed to the native workspace contract as
 * written; rejecting surrounding/control whitespace avoids silently changing
 * a developer's exact E2E target while still allowing spaces inside a name.
 */
export function isAbsoluteProjectPath(path: string): boolean {
  if (path.length === 0 || path.trim().length === 0 || path !== path.trim() || path.includes("\0") || /[\r\n\t]/.test(path)) {
    return false;
  }
  return WINDOWS_DRIVE_ABSOLUTE.test(path) || WINDOWS_UNC_ABSOLUTE.test(path) || UNIX_ABSOLUTE.test(path);
}

/**
 * Creates a fixed picker only for local Vite development, keeping release
 * builds on the official directory dialog and avoiding a second session/store.
 */
export function createDevE2eProjectPicker(environment: JaE2eEnvironment): ProjectPicker | undefined {
  const projectPath = environment.VITE_JA_E2E_PROJECT_PATH;
  if (!environment.DEV || projectPath === undefined || !isAbsoluteProjectPath(projectPath)) {
    return undefined;
  }

  /** Returns the exact validated path so E2E can choose a deterministic project. */
  const pick = async (): Promise<string> => projectPath;
  return { pick };
}
