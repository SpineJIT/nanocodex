import { describe, expect, test } from "vitest";

import { workspacePath } from "../src/sandbox-tools.js";

describe("Rivet AgentOS workspace paths", () => {
  test("confines relative and virtual workspace paths", () => {
    expect(workspacePath(".")).toBe("/workspace");
    expect(workspacePath("src/index.ts")).toBe("/workspace/src/index.ts");
    expect(workspacePath("/workspace/out.txt")).toBe("/workspace/out.txt");
    expect(() => workspacePath("../secret")).toThrow(/must not contain/);
    expect(() => workspacePath("/etc/passwd")).toThrow(/relative to/);
  });
});
