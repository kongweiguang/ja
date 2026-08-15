// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import * as CollapsiblePrimitive from "@radix-ui/react-collapsible";
import type { ComponentPropsWithoutRef, ReactElement, ReactNode } from "react";

/**
 * The Radix primitive exposes expanded state and keyboard toggling to future
 * timeline/tool groups without making the content implementation opinionated.
 */
export function Collapsible(props: ComponentPropsWithoutRef<typeof CollapsiblePrimitive.Root>): ReactElement {
  return <CollapsiblePrimitive.Root {...props} />;
}

export function CollapsibleTrigger(props: ComponentPropsWithoutRef<typeof CollapsiblePrimitive.Trigger>): ReactElement {
  return <CollapsiblePrimitive.Trigger {...props} />;
}

export function CollapsibleContent({ children, ...props }: ComponentPropsWithoutRef<typeof CollapsiblePrimitive.Content> & { children?: ReactNode }): ReactElement {
  return <CollapsiblePrimitive.Content {...props}>{children}</CollapsiblePrimitive.Content>;
}
