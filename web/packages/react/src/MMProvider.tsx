import { useMemo, type ReactNode, type ReactElement } from "react";
import { MMClient, type MMClientOptions } from "@matrixmedia/client";
import { MMClientContext } from "./context";

/** Props for {@link MMProvider}. */
export interface MMProviderProps {
  /**
   * Options used to construct an {@link MMClient}. Ignored when an explicit
   * `client` is supplied. The client is memoized on this object's identity, so
   * pass a stable reference (e.g. a module constant) to avoid reconstruction.
   */
  config?: MMClientOptions;
  /** A pre-built client to use as-is (overrides `config`). Handy for tests. */
  client?: MMClient;
  children: ReactNode;
}

/**
 * Provides a shared {@link MMClient} to all MatrixMedia React hooks and
 * components. Wrap your app (or the subtree that uses MM) in this once.
 */
export function MMProvider({
  config,
  client,
  children,
}: MMProviderProps): ReactElement {
  const value = useMemo<MMClient>(() => {
    if (client) return client;
    if (!config) {
      throw new Error(
        "MMProvider: pass either a `client` or a `config` ({ baseUrl, getToken }).",
      );
    }
    return new MMClient(config);
  }, [client, config]);

  return (
    <MMClientContext.Provider value={value}>
      {children}
    </MMClientContext.Provider>
  );
}
