import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { type } from "@tauri-apps/plugin-os";
import {
  checkAccessibilityPermission,
  requestAccessibilityPermission,
} from "tauri-plugin-macos-permissions-api";
import { commands } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { unwrapCommandResult } from "@/lib/permissions";

// Define permission state type
type PermissionState = "request" | "verify" | "granted";

// Define button configuration type
interface ButtonConfig {
  text: string;
  className: string;
}

const initializeAccessibilityInput = async (): Promise<void> => {
  const [enigoResult, shortcutsResult] = await Promise.all([
    commands.initializeEnigo(),
    commands.initializeShortcuts(),
  ]);
  unwrapCommandResult(enigoResult, "Initialize synthetic input");
  unwrapCommandResult(shortcutsResult, "Initialize shortcuts");
};

const AccessibilityPermissions: React.FC = () => {
  const { t } = useTranslation();
  const { settings } = useSettings();
  const [hasAccessibility, setHasAccessibility] = useState<boolean>(false);
  const [isAccessibilityRequired, setIsAccessibilityRequired] = useState(false);
  const [permissionState, setPermissionState] =
    useState<PermissionState>("request");
  const transcribeBinding = settings?.bindings?.transcribe?.current_binding;
  const postProcessBinding =
    settings?.bindings?.transcribe_with_post_process?.current_binding;
  const cancelBinding = settings?.bindings?.cancel?.current_binding;

  // Accessibility permissions are only required on macOS
  const isMacOS = type() === "macos";

  // Check permissions without requesting
  const checkPermissions = async (): Promise<boolean> => {
    const hasPermissions: boolean = await checkAccessibilityPermission();

    if (hasPermissions) {
      await initializeAccessibilityInput();
    }

    setHasAccessibility(hasPermissions);
    setPermissionState(hasPermissions ? "granted" : "verify");
    return hasPermissions;
  };

  // Handle the unified button action based on current state
  const handleButtonClick = async (): Promise<void> => {
    if (permissionState === "request") {
      try {
        await requestAccessibilityPermission();
        // After system prompt, transition to verification state
        setPermissionState("verify");
      } catch (error) {
        console.error("Error requesting permissions:", error);
        setPermissionState("verify");
      }
    } else if (permissionState === "verify") {
      // State is "verify" - check if permission was granted
      try {
        await checkPermissions();
      } catch (error) {
        console.error("Failed to initialize Accessibility features:", error);
        setHasAccessibility(false);
      }
    }
  };

  // On app boot - check permissions (only on macOS)
  useEffect(() => {
    if (!isMacOS) return;

    const initialSetup = async (): Promise<void> => {
      const accessibilityRequired = await commands.macosAccessibilityRequired();
      setIsAccessibilityRequired(accessibilityRequired);

      if (!accessibilityRequired) return;

      const hasPermissions: boolean = await checkAccessibilityPermission();
      if (hasPermissions) {
        await initializeAccessibilityInput();
      }

      setHasAccessibility(hasPermissions);
      setPermissionState(hasPermissions ? "granted" : "request");
    };

    initialSetup().catch((error) => {
      console.error("Failed to initialize Accessibility features:", error);
      setHasAccessibility(false);
      setPermissionState("verify");
    });
  }, [
    isMacOS,
    settings?.auto_submit,
    settings?.keyboard_implementation,
    settings?.paste_method,
    settings?.post_process_enabled,
    transcribeBinding,
    postProcessBinding,
    cancelBinding,
  ]);

  // Skip when the selected features do not use synthetic input or HandyKeys.
  if (!isMacOS || !isAccessibilityRequired || hasAccessibility) {
    return null;
  }

  // Configure button text and style based on state
  const buttonConfig: Record<PermissionState, ButtonConfig | null> = {
    request: {
      text: t("accessibility.openSettings"),
      className:
        "px-2 py-1 text-sm font-semibold bg-mid-gray/10 border  border-mid-gray/80 hover:bg-logo-primary/10 rounded cursor-pointer hover:border-logo-primary",
    },
    verify: {
      text: t("accessibility.openSettings"),
      className:
        "bg-gray-100 hover:bg-gray-200 text-gray-800 font-medium py-1 px-3 rounded-md text-sm flex items-center justify-center cursor-pointer",
    },
    granted: null,
  };

  const config = buttonConfig[permissionState] as ButtonConfig;

  return (
    <div className="p-4 w-full rounded-lg border border-mid-gray">
      <div className="flex justify-between items-center gap-2">
        <div className="">
          <p className="text-sm font-medium">
            {t("accessibility.permissionsDescription")}
          </p>
        </div>
        <button
          onClick={handleButtonClick}
          className={`min-h-10 ${config.className}`}
        >
          {config.text}
        </button>
      </div>
    </div>
  );
};

export default AccessibilityPermissions;
