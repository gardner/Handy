import { expect, test } from "@playwright/test";
import {
  accessibilityFeatureAvailable,
  autoSubmitOptionRequiresAccessibility,
  keyboardImplementationRequiresAccessibility,
  pasteMethodRequiresAccessibility,
} from "../src/lib/accessibility";

test.describe("macOS Accessibility feature policy", () => {
  test("keeps manual clipboard output available without Accessibility", () => {
    expect(pasteMethodRequiresAccessibility("macos", "none")).toBe(false);
    expect(accessibilityFeatureAvailable("macos", "denied", false)).toBe(true);
  });

  test("gates automatic paste on macOS", () => {
    expect(pasteMethodRequiresAccessibility("macos", "ctrl_v")).toBe(true);
    expect(accessibilityFeatureAvailable("macos", "denied", true)).toBe(false);
    expect(accessibilityFeatureAvailable("macos", "granted", true)).toBe(true);
  });

  test("keeps auto-submit off while gating synthetic submit keys", () => {
    expect(autoSubmitOptionRequiresAccessibility("macos", "off")).toBe(false);
    expect(autoSubmitOptionRequiresAccessibility("macos", "enter")).toBe(true);
    expect(autoSubmitOptionRequiresAccessibility("macos", "cmd_enter")).toBe(
      true,
    );
  });

  test("keeps standard shortcuts while gating HandyKeys", () => {
    expect(keyboardImplementationRequiresAccessibility("macos", "tauri")).toBe(
      false,
    );
    expect(
      keyboardImplementationRequiresAccessibility("macos", "handy_keys"),
    ).toBe(true);
  });

  test("does not gate these controls on other platforms", () => {
    expect(pasteMethodRequiresAccessibility("windows", "ctrl_v")).toBe(false);
    expect(autoSubmitOptionRequiresAccessibility("linux", "enter")).toBe(false);
    expect(
      keyboardImplementationRequiresAccessibility("windows", "handy_keys"),
    ).toBe(false);
    expect(accessibilityFeatureAvailable("windows", "denied", true)).toBe(true);
  });

  test("conservatively gates macOS features while permission is checking", () => {
    expect(accessibilityFeatureAvailable("macos", "checking", true)).toBe(
      false,
    );
  });
});
