import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-flex items-center rounded-none px-2 py-0.5 text-[10px] font-medium uppercase tracking-[0.1em] transition-colors",
  {
    variants: {
      variant: {
        default: "bg-white text-black",
        secondary: "border border-[#2a2a2a] bg-[#111111] text-[#d4d4d4]",
        outline: "border border-[#2a2a2a] text-white",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

export interface BadgeProps
  extends React.HTMLAttributes<HTMLDivElement>,
    VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, ...props }: BadgeProps) {
  return <div className={cn(badgeVariants({ variant, className }))} {...props} />;
}

export { badgeVariants };
