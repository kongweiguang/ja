// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/**
 * Combining utility classes through the installed helper avoids ad-hoc class
 * string precedence bugs when primitive states are composed by a feature.
 */
export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
