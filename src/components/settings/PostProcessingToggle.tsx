import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";
import type { KeyboardImplementation } from "@/bindings";
import { useOsType } from "@/hooks/useOsType";
import { postProcessingToggleDisabled } from "@/lib/accessibility";
import { useAccessibilityStore } from "@/stores/accessibilityStore";
import { ShortcutInput } from "./ShortcutInput";

interface PostProcessingToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const PostProcessingToggle: React.FC<PostProcessingToggleProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const osType = useOsType();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const accessibilityStatus = useAccessibilityStore((state) => state.status);

    const enabled = getSetting("post_process_enabled") || false;
    const keyboardImplementation = (getSetting("keyboard_implementation") ??
      "tauri") as KeyboardImplementation;
    const binding =
      getSetting("bindings")?.transcribe_with_post_process?.current_binding ??
      "";
    const accessibilityUnavailable = postProcessingToggleDisabled({
      osType,
      permissionStatus: accessibilityStatus,
      keyboardImplementation,
      binding,
      enabled,
    });

    return (
      <div className="flex flex-col gap-2">
        <ToggleSwitch
          checked={enabled}
          onChange={(enabled) => updateSetting("post_process_enabled", enabled)}
          disabled={accessibilityUnavailable}
          isUpdating={isUpdating("post_process_enabled")}
          label={t("settings.debug.postProcessingToggle.label")}
          description={t("settings.debug.postProcessingToggle.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
        />
        {accessibilityUnavailable && accessibilityStatus === "denied" && (
          /* The post-processing page is hidden while disabled. Keep the
             inactive binding reachable here so a denied user can replace a
             media key with an ordinary Tauri shortcut, then enable it. The
             shortcut control supplies the Accessibility note. */
          <ShortcutInput
            shortcutId="transcribe_with_post_process"
            descriptionMode={descriptionMode}
            grouped={grouped}
          />
        )}
      </div>
    );
  });
