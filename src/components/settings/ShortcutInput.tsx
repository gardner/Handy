import React from "react";
import { useSettings } from "../../hooks/useSettings";
import { GlobalShortcutInput } from "./GlobalShortcutInput";
import { HandyKeysShortcutInput } from "./HandyKeysShortcutInput";
import type { KeyboardImplementation } from "@/bindings";
import { useOsType } from "@/hooks/useOsType";
import {
  accessibilityFeatureAvailable,
  shortcutBindingRequiresAccessibility,
  shortcutEditorRequiresAccessibility,
} from "@/lib/accessibility";
import { useAccessibilityStore } from "@/stores/accessibilityStore";
import { AccessibilityRequiredNote } from "./AccessibilityRequiredNote";

interface ShortcutInputProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
  shortcutId: string;
  disabled?: boolean;
}

/**
 * Wrapper component that selects the appropriate shortcut input implementation
 * based on the keyboard_implementation setting.
 *
 * - "tauri" (default): Uses GlobalShortcutInput with JS keyboard events
 * - "handy_keys": Uses HandyKeysShortcutInput with backend key events
 */
export const ShortcutInput: React.FC<ShortcutInputProps> = (props) => {
  const { getSetting } = useSettings();
  const osType = useOsType();
  const accessibilityStatus = useAccessibilityStore((state) => state.status);
  const keyboardImplementation = (getSetting("keyboard_implementation") ??
    "tauri") as KeyboardImplementation;
  const currentBinding =
    getSetting("bindings")?.[props.shortcutId]?.current_binding ?? "";
  const bindingAccessibilityUnavailable = !accessibilityFeatureAvailable(
    osType,
    accessibilityStatus,
    shortcutBindingRequiresAccessibility(
      osType,
      keyboardImplementation,
      currentBinding,
    ),
  );
  const editorAccessibilityUnavailable = !accessibilityFeatureAvailable(
    osType,
    accessibilityStatus,
    shortcutEditorRequiresAccessibility(osType, keyboardImplementation),
  );
  const inputProps = {
    ...props,
    disabled: props.disabled || editorAccessibilityUnavailable,
  };

  // Default to Tauri implementation if not set
  const input =
    keyboardImplementation === "handy_keys" ? (
      <HandyKeysShortcutInput {...inputProps} />
    ) : (
      <GlobalShortcutInput {...inputProps} />
    );

  return (
    <>
      {input}
      {bindingAccessibilityUnavailable &&
        osType === "macos" &&
        accessibilityStatus === "denied" && (
          <div className="px-4 py-2">
            <AccessibilityRequiredNote />
          </div>
        )}
    </>
  );
};
