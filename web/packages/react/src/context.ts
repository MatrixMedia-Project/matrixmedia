import { createContext } from "react";
import type { MMClient } from "@matrixmedia/client";

/**
 * React context holding the shared {@link MMClient}. `null` until an
 * {@link MMProvider} supplies one; hooks throw a clear error when read outside
 * a provider.
 */
export const MMClientContext = createContext<MMClient | null>(null);
