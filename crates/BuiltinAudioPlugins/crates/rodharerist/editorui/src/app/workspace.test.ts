import { describe, expect, test } from "bun:test";
import {
  parseWorkspacePath,
  workspaceHref,
  workspaceSuffix,
} from "./workspace";

describe("workspace routing", () => {
  test("defaults an instance path without a workspace to rig", () => {
    expect(parseWorkspacePath("/instance/track-1::insert-1")).toEqual({
      mode: "rig",
    });
    expect(workspaceSuffix("/instance/track-1::insert-1")).toBe("/rig");
  });

  test("keeps browse section and never splits the instance id", () => {
    const path = "/instance/track-1::insert-track-1-1/browse/explore";
    expect(parseWorkspacePath(path)).toEqual({
      mode: "browse",
      section: "explore",
    });
    expect(workspaceHref("track-1::insert-track-1-1", parseWorkspacePath(path))).toBe(
      path,
    );
  });

  test("preview paths without an instance still resolve", () => {
    expect(parseWorkspacePath("/browse/nam")).toEqual({
      mode: "browse",
      section: "nam",
    });
    expect(workspaceHref(null, { mode: "browse", section: "ir" })).toBe(
      "/browse/ir",
    );
  });

  test("unknown browse sections fall back to presets", () => {
    expect(parseWorkspacePath("/instance/abc/browse/marketplace")).toEqual({
      mode: "browse",
      section: "presets",
    });
  });
});
