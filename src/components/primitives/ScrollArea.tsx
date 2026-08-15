// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import * as ScrollAreaPrimitive from "@radix-ui/react-scroll-area";
import type { ComponentPropsWithoutRef, ReactElement, ReactNode } from "react";
import { cn } from "./cn";

/**
 * Radix's viewport preserves keyboard and wheel semantics across platform
 * WebViews, so feature panels do not need bespoke overflow behavior.
 */
export function ScrollArea({ className, children, ...props }: ComponentPropsWithoutRef<typeof ScrollAreaPrimitive.Root> & { children?: ReactNode }): ReactElement {
  return (
    <ScrollAreaPrimitive.Root {...props} className={cn("ja-scroll-area", className)}>
      <ScrollAreaPrimitive.Viewport className="ja-scroll-area-viewport">{children}</ScrollAreaPrimitive.Viewport>
      <ScrollAreaPrimitive.Scrollbar orientation="vertical" className="ja-scrollbar">
        <ScrollAreaPrimitive.Thumb className="ja-scrollbar-thumb" />
      </ScrollAreaPrimitive.Scrollbar>
      <ScrollAreaPrimitive.Scrollbar orientation="horizontal" className="ja-scrollbar">
        <ScrollAreaPrimitive.Thumb className="ja-scrollbar-thumb" />
      </ScrollAreaPrimitive.Scrollbar>
      <ScrollAreaPrimitive.Corner />
    </ScrollAreaPrimitive.Root>
  );
}
