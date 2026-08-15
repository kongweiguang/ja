// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import type { ButtonHTMLAttributes, ReactElement, ReactNode } from "react";
import { Button } from "./Button";
import { cn } from "./cn";

export interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  label: string;
  children: ReactNode;
}

/**
 * An explicit accessible label is required because icon-only actions otherwise
 * become unusable to keyboard and screen-reader users.
 */
export function IconButton({ label, className, children, ...props }: IconButtonProps): ReactElement {
  return (
    <Button
      {...props}
      variant="ghost"
      size="sm"
      className={cn("ja-icon-button", className)}
      aria-label={label}
    >
      {children}
    </Button>
  );
}
