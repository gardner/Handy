import { expect, test } from "@playwright/test";
import {
  hasRequiredMacOSPermissions,
  unwrapCommandResult,
} from "../src/lib/permissions";

test.describe("macOS permission gating", () => {
  test("allows microphone-only profiles without Accessibility", () => {
    expect(
      hasRequiredMacOSPermissions({
        microphoneGranted: true,
        accessibilityGranted: false,
        accessibilityRequired: false,
      }),
    ).toBe(true);
  });

  test("requires Accessibility when a selected feature needs it", () => {
    expect(
      hasRequiredMacOSPermissions({
        microphoneGranted: true,
        accessibilityGranted: false,
        accessibilityRequired: true,
      }),
    ).toBe(false);
  });

  test("always requires microphone access", () => {
    expect(
      hasRequiredMacOSPermissions({
        microphoneGranted: false,
        accessibilityGranted: true,
        accessibilityRequired: false,
      }),
    ).toBe(false);
  });
});

test.describe("Tauri command results", () => {
  test("returns successful command data", () => {
    expect(
      unwrapCommandResult({ status: "ok", data: "ready" }, "initialize"),
    ).toBe("ready");
  });

  test("throws resolved backend errors", () => {
    expect(() =>
      unwrapCommandResult(
        { status: "error", error: "permission denied" },
        "initialize",
      ),
    ).toThrow("initialize: permission denied");
  });
});
