import { useState } from "react";
import { useTranslation } from "react-i18next";
import { requestAccessibilityPermission } from "tauri-plugin-macos-permissions-api";
import { commands } from "@/bindings";
import { useAccessibility } from "@/hooks/useAccessibility";
import { notify } from "@/stores/noticeStore";
import { Button } from "../ui/Button";

export function AccessibilityNote() {
  const { t } = useTranslation();
  const granted = useAccessibility();
  const [opening, setOpening] = useState(false);
  if (granted) return null;
  const enable = async () => {
    setOpening(true);
    try {
      const result = await commands.resetMacosPermission("accessibility");
      if (result.status === "error") throw new Error(result.error);
      await requestAccessibilityPermission();
    } catch {
      notify({ title: t("onboarding.permissions.errors.requestFailed") });
    } finally {
      setOpening(false);
    }
  };
  return (
    <aside className="max-w-3xl w-full rounded-lg border border-line bg-wash p-4 text-sm space-y-2">
      <p className="font-semibold">{t("settings.accessibility.title")}</p>
      <p>{t("settings.accessibility.description")}</p>
      <p>{t("settings.accessibility.enableLater")}</p>
      <Button
        variant="primary-soft"
        size="sm"
        disabled={opening}
        onClick={enable}
      >
        {t("onboarding.permissions.openSettings")}
      </Button>
    </aside>
  );
}
