import React from "react";
import { useSettings } from "../../hooks/useSettings";
import { GlobalShortcutInput } from "./GlobalShortcutInput";
import { HandyKeysShortcutInput } from "./HandyKeysShortcutInput";
import { useOsType } from "@/hooks/useOsType";
import {
  accessibilityFeatureAvailable,
  keyboardImplementationRequiresAccessibility,
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
  const keyboardImplementation = getSetting("keyboard_implementation");

  // Default to Tauri implementation if not set
  if (keyboardImplementation === "handy_keys") {
    const accessibilityUnavailable = !accessibilityFeatureAvailable(
      osType,
      accessibilityStatus,
      keyboardImplementationRequiresAccessibility(
        osType,
        keyboardImplementation,
      ),
    );

    return (
      <>
        <HandyKeysShortcutInput
          {...props}
          disabled={props.disabled || accessibilityUnavailable}
        />
        {accessibilityUnavailable &&
          osType === "macos" &&
          accessibilityStatus === "denied" && (
            <div className="px-4 py-2">
              <AccessibilityRequiredNote />
            </div>
          )}
      </>
    );
  }

  return <GlobalShortcutInput {...props} />;
};
