import { Info } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { requestAccessibilityPermission } from "tauri-plugin-macos-permissions-api";
import { useAccessibilityStore } from "@/stores/accessibilityStore";

export const AccessibilityRequiredNote: React.FC = () => {
  const { t } = useTranslation();
  const [isOpeningSettings, setIsOpeningSettings] = useState(false);
  const refreshPermission = useAccessibilityStore(
    (state) => state.refreshPermission,
  );

  const handleOpenSettings = async () => {
    setIsOpeningSettings(true);
    try {
      await requestAccessibilityPermission();
      await refreshPermission();
    } catch (error) {
      console.error("Failed to open Accessibility settings:", error);
    } finally {
      setIsOpeningSettings(false);
    }
  };

  return (
    <div
      role="note"
      className="flex items-start gap-1.5 text-xs text-mid-gray max-w-[280px]"
    >
      <Info className="w-3.5 h-3.5 mt-0.5 shrink-0" aria-hidden="true" />
      <span>
        {t("accessibility.permissionsDescription")}{" "}
        <button
          type="button"
          onClick={handleOpenSettings}
          disabled={isOpeningSettings}
          className="text-logo-primary hover:underline disabled:opacity-50"
        >
          {t("accessibility.openSettings")}
        </button>
      </span>
    </div>
  );
};
