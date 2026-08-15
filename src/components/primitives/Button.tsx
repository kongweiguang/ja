// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later
/* eslint-disable react-refresh/only-export-components */

import { Slot } from "@radix-ui/react-slot";
import { cva, type VariantProps } from "class-variance-authority";
import type { ButtonHTMLAttributes, ReactElement } from "react";
import { cn } from "./cn";

const buttonVariants = cva("ja-button", {
  variants: {
    variant: {
      primary: "ja-button-primary",
      secondary: "ja-button-secondary",
      ghost: "ja-button-ghost",
      danger: "ja-button-danger",
    },
    size: {
      sm: "ja-button-sm",
      md: "ja-button-md",
      lg: "ja-button-lg",
    },
  },
  defaultVariants: { variant: "primary", size: "md" },
});

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement>, VariantProps<typeof buttonVariants> {
  asChild?: boolean;
  loading?: boolean;
}

/**
 * Shared button semantics keep focus and pending behavior identical across
 * sidecar controls, including destructive approval actions.
 */
export function Button({ className, variant, size, asChild = false, loading = false, disabled, children, ...props }: ButtonProps): ReactElement {
  const Component = asChild ? Slot : "button";
  return (
    <Component
      className={cn(buttonVariants({ variant, size }), className)}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      {...props}
    >
      {loading ? "处理中…" : children}
    </Component>
  );
}

export { buttonVariants };
