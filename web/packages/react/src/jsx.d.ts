import type { DetailedHTMLProps, HTMLAttributes } from "react";

/** Attributes accepted by the `<mm-stream>` custom element (see @matrixmedia/widget). */
export interface MMStreamElementAttributes
  extends DetailedHTMLProps<HTMLAttributes<HTMLElement>, HTMLElement> {
  room?: string;
  server?: string;
  token?: string;
}

declare global {
  namespace JSX {
    interface IntrinsicElements {
      "mm-stream": MMStreamElementAttributes;
    }
  }
}
