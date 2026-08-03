import { describe, expect, it } from "vitest";

import { workspacePath } from "../workflows/sandbox-tools";

describe("Vercel Sandbox workspace paths", () => {
  it("confines relative and virtual workspace paths", () => {
    expect(workspacePath(".")).toBe("/vercel/sandbox");
    expect(workspacePath("src/index.ts")).toBe("/vercel/sandbox/src/index.ts");
    expect(workspacePath("/workspace/out.txt")).toBe("/vercel/sandbox/out.txt");
    expect(() => workspacePath("../secret")).toThrow(/must not contain/);
    expect(() => workspacePath("/etc/passwd")).toThrow(/relative to/);
  });
});
