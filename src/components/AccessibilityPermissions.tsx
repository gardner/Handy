import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { type } from "@tauri-apps/plugin-os";
import { requestAccessibilityPermission } from "tauri-plugin-macos-permissions-api";
import { commands } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { useAccessibilityStore } from "@/stores/accessibilityStore";

const AccessibilityPermissions: React.FC = () => {
  const { t } = useTranslation();
  const { settings } = useSettings();
  const [isAccessibilityRequired, setIsAccessibilityRequired] = useState(false);
  const [isOpeningSettings, setIsOpeningSettings] = useState(false);
  const accessibilityStatus = useAccessibilityStore((state) => state.status);
  const refreshAccessibilityPermission = useAccessibilityStore(
    (state) => state.refreshPermission,
  );
  const isMacOS = type() === "macos";

  const refreshPermissionState = useCallback(async () => {
    const required = await commands.macosAccessibilityRequired();
    setIsAccessibilityRequired(required);
    await refreshAccessibilityPermission();
  }, [refreshAccessibilityPermission]);

  // Re-evaluate the backend's authoritative policy whenever an input setting
  // changes. Native lifecycles stay backend-owned; this component only mirrors
  // permission state and explains unavailable controls.
  useEffect(() => {
    if (!isMacOS) return;

    refreshPermissionState().catch((error) => {
      console.error("Failed to refresh Accessibility state:", error);
    });
  }, [
    isMacOS,
    refreshPermissionState,
    settings?.auto_submit,
    settings?.keyboard_implementation,
    settings?.paste_method,
    settings?.post_process_enabled,
    settings?.bindings,
  ]);

  // System Settings is outside the app, so refresh when focus returns.
  useEffect(() => {
    if (!isMacOS) return;

    const handleFocus = () => {
      refreshPermissionState().catch((error) => {
        console.error("Failed to refresh Accessibility state:", error);
      });
    };

    window.addEventListener("focus", handleFocus);
    return () => window.removeEventListener("focus", handleFocus);
  }, [isMacOS, refreshPermissionState]);

  const handleOpenSettings = async () => {
    setIsOpeningSettings(true);
    try {
      await requestAccessibilityPermission();
      await refreshPermissionState();
    } catch (error) {
      console.error("Failed to open Accessibility settings:", error);
    } finally {
      setIsOpeningSettings(false);
    }
  };

  if (
    !isMacOS ||
    !isAccessibilityRequired ||
    accessibilityStatus === "checking" ||
    accessibilityStatus === "granted"
  ) {
    return null;
  }

  return (
    <div className="p-4 w-full rounded-lg border border-mid-gray">
      <div className="flex justify-between items-center gap-2">
        <p className="text-sm font-medium">
          {t("accessibility.permissionsDescription")}
        </p>
        <button
          type="button"
          onClick={handleOpenSettings}
          disabled={isOpeningSettings}
          className="min-h-10 px-2 py-1 text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 hover:bg-logo-primary/10 rounded cursor-pointer hover:border-logo-primary disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {t("accessibility.openSettings")}
        </button>
      </div>
    </div>
  );
};

export default AccessibilityPermissions;
