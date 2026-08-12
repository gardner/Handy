import { expect, test } from "@playwright/test";
import {
  accessibilityFeatureAvailable,
  autoSubmitOptionRequiresAccessibility,
  createInputInitializationPlan,
  keyboardImplementationRequiresAccessibility,
  pasteMethodRequiresAccessibility,
  postProcessingToggleDisabled,
  shortcutBindingRequiresAccessibility,
  shortcutEditorRequiresAccessibility,
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

  test("keeps ordinary Tauri shortcuts while gating HandyKeys", () => {
    expect(keyboardImplementationRequiresAccessibility("macos", "tauri")).toBe(
      false,
    );
    expect(
      keyboardImplementationRequiresAccessibility("macos", "handy_keys"),
    ).toBe(true);
  });

  test("gates only Tauri shortcuts backed by the macOS media-key event tap", () => {
    expect(
      shortcutBindingRequiresAccessibility(
        "macos",
        "tauri",
        "CommandOrControl+Shift+Space",
      ),
    ).toBe(false);
    expect(
      shortcutBindingRequiresAccessibility("macos", "tauri", "MediaPlayPause"),
    ).toBe(true);
    expect(
      shortcutBindingRequiresAccessibility(
        "macos",
        "tauri",
        "Shift+MediaTrackNext",
      ),
    ).toBe(true);
    expect(
      shortcutBindingRequiresAccessibility(
        "windows",
        "tauri",
        "MediaPlayPause",
      ),
    ).toBe(false);
  });

  test("keeps the Tauri editor available to replace a restricted binding", () => {
    expect(shortcutEditorRequiresAccessibility("macos", "tauri")).toBe(false);
    expect(shortcutEditorRequiresAccessibility("macos", "handy_keys")).toBe(
      true,
    );
  });

  test("gates post-processing activation but still permits turning it off", () => {
    const deniedMediaKey = {
      osType: "macos" as const,
      permissionStatus: "denied" as const,
      keyboardImplementation: "tauri" as const,
      binding: "MediaTrackNext",
    };

    expect(
      postProcessingToggleDisabled({
        ...deniedMediaKey,
        enabled: false,
      }),
    ).toBe(true);
    expect(
      postProcessingToggleDisabled({
        ...deniedMediaKey,
        enabled: true,
      }),
    ).toBe(false);
    expect(
      postProcessingToggleDisabled({
        ...deniedMediaKey,
        keyboardImplementation: "handy_keys",
        binding: "Option+Space",
        enabled: false,
      }),
    ).toBe(true);
    expect(
      postProcessingToggleDisabled({
        ...deniedMediaKey,
        binding: "CommandOrControl+Shift+Space",
        enabled: false,
      }),
    ).toBe(false);
  });

  test("initializes only the native input capabilities selected by the profile", () => {
    expect(
      createInputInitializationPlan({
        osType: "macos",
        pasteMethod: "none",
        autoSubmit: false,
        accessibilityGranted: false,
      }),
    ).toEqual({ shortcuts: true, syntheticInput: false });

    // HandyKeys is initialized by the shortcut backend; clipboard-only output
    // must not construct Enigo just because another feature needs Accessibility.
    expect(
      createInputInitializationPlan({
        osType: "macos",
        pasteMethod: "none",
        autoSubmit: false,
        accessibilityGranted: true,
      }),
    ).toEqual({ shortcuts: true, syntheticInput: false });

    expect(
      createInputInitializationPlan({
        osType: "macos",
        pasteMethod: "ctrl_v",
        autoSubmit: false,
        accessibilityGranted: true,
      }),
    ).toEqual({ shortcuts: true, syntheticInput: true });

    expect(
      createInputInitializationPlan({
        osType: "macos",
        pasteMethod: "ctrl_v",
        autoSubmit: false,
        accessibilityGranted: false,
      }),
    ).toEqual({ shortcuts: true, syntheticInput: false });
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
