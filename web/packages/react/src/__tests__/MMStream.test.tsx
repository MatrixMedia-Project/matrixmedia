import { describe, it, expect } from "vitest";
import { render, waitFor } from "@testing-library/react";
import { MMStream } from "../components/MMStream";

describe("<MMStream>", () => {
  it("renders the mm-stream custom element with the right attributes", async () => {
    const { container } = render(
      <MMStream
        room="!abc:matrix.example.com"
        server="https://matrix.example.com"
        token="tok"
      />,
    );

    // The widget import resolves to the local workspace package; once it loads
    // the tag is emitted. (If it were absent a fallback alert would render.)
    await waitFor(() => {
      expect(container.querySelector("mm-stream")).toBeTruthy();
    });

    const el = container.querySelector("mm-stream")!;
    expect(el.getAttribute("room")).toBe("!abc:matrix.example.com");
    expect(el.getAttribute("server")).toBe("https://matrix.example.com");
    expect(el.getAttribute("token")).toBe("tok");
  });
});
