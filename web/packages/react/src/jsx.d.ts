import type { DetailedHTMLProps, HTMLAttributes } from "react";

/** Attributes accepted by the `<mm-stream>` custom element (see @matrixmedia/widget). */
export interface MMStreamElementAttributes
  extends DetailedHTMLProps<HTMLAttributes<HTMLElement>, HTMLElement> {
  room?: string;
  server?: string;
  token?: string;
}

// Augment React's own JSX namespace (not the global one). React 19's
// @types removed the global `JSX` namespace; `React.JSX` exists on both
// @types/react 18 and 19, so this augmentation works across the peer range.
declare module "react" {
  namespace JSX {
    interface IntrinsicElements {
      "mm-stream": MMStreamElementAttributes;
    }
  }
}
