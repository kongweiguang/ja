// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import * as TooltipPrimitive from "@radix-ui/react-tooltip";
import type { ComponentPropsWithoutRef, ReactElement, ReactNode } from "react";
import { cn } from "./cn";

/**
 * The provider centralizes the delay and portal behavior so tooltips do not
 * create independent timers for every toolbar button.
 */
export function TooltipProvider({ children }: { children: ReactNode }): ReactElement {
  return <TooltipPrimitive.Provider delayDuration={350}>{children}</TooltipPrimitive.Provider>;
}

/**
 * Radix handles focus, escape, and pointer interactions while this wrapper
 * supplies the semantic token class used by both themes.
 */
export function Tooltip({ children }: { children: ReactNode }): ReactElement {
  return <TooltipPrimitive.Root>{children}</TooltipPrimitive.Root>;
}

export function TooltipTrigger(props: ComponentPropsWithoutRef<typeof TooltipPrimitive.Trigger>): ReactElement {
  return <TooltipPrimitive.Trigger {...props} asChild />;
}

export function TooltipContent({ className, sideOffset = 6, ...props }: ComponentPropsWithoutRef<typeof TooltipPrimitive.Content>): ReactElement {
  return (
    <TooltipPrimitive.Portal>
      <TooltipPrimitive.Content {...props} sideOffset={sideOffset} className={cn("ja-tooltip-content", className)} />
    </TooltipPrimitive.Portal>
  );
}
