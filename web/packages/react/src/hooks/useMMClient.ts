import { useContext } from "react";
import type { MMClient } from "@matrixmedia/client";
import { MMClientContext } from "../context";

/**
 * Read the shared {@link MMClient} from context. Throws if used outside an
 * {@link MMProvider}.
 */
export function useMMClient(): MMClient {
  const client = useContext(MMClientContext);
  if (!client) {
    throw new Error(
      "useMMClient must be used within an <MMProvider>. Wrap your app in <MMProvider config={{ baseUrl, getToken }}>.",
    );
  }
  return client;
}
