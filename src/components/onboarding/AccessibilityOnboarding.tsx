import { useEffect, useState, useCallback, useRef } from "react";
import { useTranslation } from "react-i18next";
import { platform } from "@tauri-apps/plugin-os";
import {
  checkAccessibilityPermission,
  requestAccessibilityPermission,
  checkMicrophonePermission,
  requestMicrophonePermission,
} from "tauri-plugin-macos-permissions-api";
import { toast } from "sonner";
import { commands } from "@/bindings";
import {
  hasRequiredMacOSPermissions,
  unwrapCommandResult,
} from "@/lib/permissions";
import { useSettingsStore } from "@/stores/settingsStore";
import { useAccessibilityStore } from "@/stores/accessibilityStore";
import HandyTextLogo from "../icons/HandyTextLogo";
import { AlertTriangle, Keyboard, Mic, Check, Loader2 } from "lucide-react";

interface AccessibilityOnboardingProps {
  onComplete: () => void;
}

type PermissionStatus = "checking" | "needed" | "waiting" | "granted";
type PermissionPlatform = "macos" | "windows" | "other";

interface PermissionsState {
  accessibility: PermissionStatus;
  microphone: PermissionStatus;
}

const AccessibilityOnboarding: React.FC<AccessibilityOnboardingProps> = ({
  onComplete,
}) => {
  const { t } = useTranslation();
  const refreshAudioDevices = useSettingsStore(
    (state) => state.refreshAudioDevices,
  );
  const refreshOutputDevices = useSettingsStore(
    (state) => state.refreshOutputDevices,
  );
  const refreshSettings = useSettingsStore((state) => state.refreshSettings);
  const [permissionPlatform, setPermissionPlatform] =
    useState<PermissionPlatform | null>(null);
  const [accessibilityRequired, setAccessibilityRequired] = useState<
    boolean | null
  >(null);
  const [permissions, setPermissions] = useState<PermissionsState>({
    accessibility: "checking",
    microphone: "checking",
  });
  const [isApplyingMinimumPermissions, setIsApplyingMinimumPermissions] =
    useState(false);
  const [showMinimumPermissionsWarning, setShowMinimumPermissionsWarning] =
    useState(false);
  const pollingRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const errorCountRef = useRef<number>(0);
  const MAX_POLLING_ERRORS = 3;

  const isMacOS = permissionPlatform === "macos";
  const isWindows = permissionPlatform === "windows";
  const showMicrophonePermission = isMacOS || isWindows;
  const showAccessibilityPermission = isMacOS && accessibilityRequired === true;

  const allGranted = isMacOS
    ? accessibilityRequired !== null &&
      hasRequiredMacOSPermissions({
        microphoneGranted: permissions.microphone === "granted",
        accessibilityGranted: permissions.accessibility === "granted",
        accessibilityRequired,
      })
    : isWindows
      ? permissions.microphone === "granted"
      : true;

  const completeOnboarding = useCallback(async () => {
    await Promise.all([
      refreshAudioDevices(),
      refreshOutputDevices(),
      refreshSettings(),
    ]);
    timeoutRef.current = setTimeout(() => onComplete(), 300);
  }, [onComplete, refreshAudioDevices, refreshOutputDevices, refreshSettings]);

  const hasWindowsMicrophoneAccess = useCallback(async (): Promise<boolean> => {
    const microphoneStatus =
      await commands.getWindowsMicrophonePermissionStatus();

    if (!microphoneStatus.supported) {
      return true;
    }

    return microphoneStatus.overall_access !== "denied";
  }, []);

  // Check platform and permission status on mount
  useEffect(() => {
    const currentPlatform = platform();
    const nextPlatform: PermissionPlatform =
      currentPlatform === "macos"
        ? "macos"
        : currentPlatform === "windows"
          ? "windows"
          : "other";

    setPermissionPlatform(nextPlatform);

    // Skip immediately on unsupported platforms
    if (nextPlatform === "other") {
      onComplete();
      return;
    }

    const checkInitial = async () => {
      if (nextPlatform === "macos") {
        try {
          const [requiresAccessibility, microphoneGranted] = await Promise.all([
            commands.macosAccessibilityRequired(),
            checkMicrophonePermission(),
          ]);
          const accessibilityGranted = requiresAccessibility
            ? await checkAccessibilityPermission()
            : false;
          if (requiresAccessibility) {
            useAccessibilityStore
              .getState()
              .setPermissionGranted(accessibilityGranted);
          }
          setAccessibilityRequired(requiresAccessibility);

          // Initialize Accessibility-backed input only when it is selected.
          if (requiresAccessibility && accessibilityGranted) {
            try {
              const [enigoResult, shortcutsResult] = await Promise.all([
                commands.initializeEnigo(),
                commands.initializeShortcuts(),
              ]);
              unwrapCommandResult(enigoResult, "Initialize synthetic input");
              unwrapCommandResult(shortcutsResult, "Initialize shortcuts");
            } catch (e) {
              console.warn("Failed to initialize after permission grant:", e);
            }
          }

          const newState: PermissionsState = {
            accessibility:
              !requiresAccessibility || accessibilityGranted
                ? "granted"
                : "needed",
            microphone: microphoneGranted ? "granted" : "needed",
          };

          setPermissions(newState);

          if (
            (!requiresAccessibility || accessibilityGranted) &&
            microphoneGranted
          ) {
            await completeOnboarding();
          }
        } catch (error) {
          console.error("Failed to check macOS permissions:", error);
          toast.error(t("onboarding.permissions.errors.checkFailed"));
          setAccessibilityRequired(true);
          setPermissions({
            accessibility: "needed",
            microphone: "needed",
          });
        }

        return;
      }

      try {
        const microphoneGranted = await hasWindowsMicrophoneAccess();

        setPermissions({
          accessibility: "granted",
          microphone: microphoneGranted ? "granted" : "needed",
        });

        if (microphoneGranted) {
          await completeOnboarding();
        }
      } catch (error) {
        console.warn("Failed to check Windows microphone permissions:", error);
        setPermissions({
          accessibility: "granted",
          microphone: "granted",
        });
        await completeOnboarding();
      }
    };

    checkInitial();
  }, [completeOnboarding, hasWindowsMicrophoneAccess, onComplete, t]);

  // Polling for permissions after user clicks a button
  const startPolling = useCallback(() => {
    if (pollingRef.current || permissionPlatform === null) return;

    pollingRef.current = setInterval(async () => {
      try {
        if (permissionPlatform === "windows") {
          const microphoneGranted = await hasWindowsMicrophoneAccess();

          if (microphoneGranted) {
            setPermissions((prev) => ({ ...prev, microphone: "granted" }));

            if (pollingRef.current) {
              clearInterval(pollingRef.current);
              pollingRef.current = null;
            }

            await completeOnboarding();
          }

          errorCountRef.current = 0;
          return;
        }

        const microphoneGranted = await checkMicrophonePermission();
        const accessibilityGranted = accessibilityRequired
          ? await checkAccessibilityPermission()
          : false;
        if (accessibilityRequired) {
          useAccessibilityStore
            .getState()
            .setPermissionGranted(accessibilityGranted);
        }

        setPermissions((prev) => {
          const newState = { ...prev };

          if (
            accessibilityRequired &&
            accessibilityGranted &&
            prev.accessibility !== "granted"
          ) {
            newState.accessibility = "granted";
            // Initialize Enigo and shortcuts when accessibility is granted
            Promise.all([
              commands.initializeEnigo(),
              commands.initializeShortcuts(),
            ])
              .then(([enigoResult, shortcutsResult]) => {
                unwrapCommandResult(enigoResult, "Initialize synthetic input");
                unwrapCommandResult(shortcutsResult, "Initialize shortcuts");
              })
              .catch((e) => {
                console.warn("Failed to initialize after permission grant:", e);
              });
          }

          if (microphoneGranted && prev.microphone !== "granted") {
            newState.microphone = "granted";
          }

          return newState;
        });

        // If both granted, stop polling, refresh audio devices, and proceed
        if (
          (!accessibilityRequired || accessibilityGranted) &&
          microphoneGranted
        ) {
          if (pollingRef.current) {
            clearInterval(pollingRef.current);
            pollingRef.current = null;
          }
          await completeOnboarding();
        }

        // Reset error count on success
        errorCountRef.current = 0;
      } catch (error) {
        console.error("Error checking permissions:", error);
        errorCountRef.current += 1;

        if (errorCountRef.current >= MAX_POLLING_ERRORS) {
          // Stop polling after too many consecutive errors
          if (pollingRef.current) {
            clearInterval(pollingRef.current);
            pollingRef.current = null;
          }
          toast.error(t("onboarding.permissions.errors.checkFailed"));
        }
      }
    }, 1000);
  }, [
    accessibilityRequired,
    completeOnboarding,
    hasWindowsMicrophoneAccess,
    permissionPlatform,
    t,
  ]);

  // Cleanup polling and timeouts on unmount
  useEffect(() => {
    return () => {
      if (pollingRef.current) {
        clearInterval(pollingRef.current);
      }
      if (timeoutRef.current) {
        clearTimeout(timeoutRef.current);
      }
    };
  }, []);

  const handleGrantAccessibility = async () => {
    try {
      setShowMinimumPermissionsWarning(false);
      await requestAccessibilityPermission();
      setPermissions((prev) => ({ ...prev, accessibility: "waiting" }));
      startPolling();
    } catch (error) {
      console.error("Failed to request accessibility permission:", error);
      toast.error(t("onboarding.permissions.errors.requestFailed"));
    }
  };

  const handleGrantMicrophone = async () => {
    try {
      if (isWindows) {
        await commands.openMicrophonePrivacySettings();
      } else {
        await requestMicrophonePermission();
      }

      setPermissions((prev) => ({ ...prev, microphone: "waiting" }));
      startPolling();
    } catch (error) {
      console.error("Failed to request microphone permission:", error);
      toast.error(t("onboarding.permissions.errors.requestFailed"));
    }
  };

  const handleUseMinimumPermissions = async () => {
    setIsApplyingMinimumPermissions(true);

    try {
      const result = await commands.configureMinimumPermissions();
      if (result.status === "error") {
        throw new Error(String(result.error));
      }

      if (pollingRef.current) {
        clearInterval(pollingRef.current);
        pollingRef.current = null;
      }

      if (result.data.reset_bindings.length > 0) {
        toast.warning(t("settings.debug.keyboardImplementation.bindingsReset"));
      }

      useAccessibilityStore.getState().setPermissionGranted(false);
      await completeOnboarding();
    } catch (error) {
      console.error("Failed to enable minimum-permissions mode:", error);
      toast.error(t("onboarding.permissions.errors.requestFailed"));
      setIsApplyingMinimumPermissions(false);
    }
  };

  const isChecking =
    permissionPlatform === null ||
    (isMacOS &&
      (accessibilityRequired === null ||
        (permissions.accessibility === "checking" &&
          permissions.microphone === "checking"))) ||
    (isWindows && permissions.microphone === "checking");

  // Still checking platform/initial permissions
  if (isChecking) {
    return (
      <div className="h-screen w-screen flex items-center justify-center">
        <Loader2 className="w-8 h-8 animate-spin text-text/50" />
      </div>
    );
  }

  // All permissions granted - show success briefly
  if (allGranted) {
    return (
      <div className="h-screen w-screen flex flex-col items-center justify-center gap-4">
        <div className="p-4 rounded-full bg-emerald-500/20">
          <Check className="w-12 h-12 text-emerald-400" />
        </div>
        <p className="text-lg font-medium text-text">
          {t("onboarding.permissions.allGranted")}
        </p>
      </div>
    );
  }

  // Show permissions request screen
  return (
    <div className="h-screen w-screen flex flex-col p-6 gap-6 items-center justify-center">
      <div className="flex flex-col items-center gap-2">
        <HandyTextLogo width={200} />
      </div>

      <div className="max-w-md w-full flex flex-col items-center gap-4">
        <div className="text-center mb-2">
          <h2 className="text-xl font-semibold text-text mb-2">
            {t("onboarding.permissions.title")}
          </h2>
          <p className="text-text/70">
            {t("onboarding.permissions.description")}
          </p>
        </div>

        {/* Microphone Permission Card */}
        {showMicrophonePermission && (
          <div className="w-full p-4 rounded-lg bg-white/5 border border-mid-gray/20">
            <div className="flex items-center gap-4">
              <div className="p-3 rounded-full bg-logo-primary/20 shrink-0">
                <Mic className="w-6 h-6 text-logo-primary" />
              </div>
              <div className="flex-1 min-w-0">
                <h3 className="font-medium text-text">
                  {t("onboarding.permissions.microphone.title")}
                </h3>
                <p className="text-sm text-text/60 mb-3">
                  {t("onboarding.permissions.microphone.description")}
                </p>
                {permissions.microphone === "granted" ? (
                  <div className="flex items-center gap-2 text-emerald-400 text-sm">
                    <Check className="w-4 h-4" />
                    {t("onboarding.permissions.granted")}
                  </div>
                ) : permissions.microphone === "waiting" ? (
                  <div className="flex items-center gap-2 text-text/50 text-sm">
                    <Loader2 className="w-4 h-4 animate-spin" />
                    {t("onboarding.permissions.waiting")}
                  </div>
                ) : (
                  <button
                    onClick={handleGrantMicrophone}
                    className="px-4 py-2 rounded-lg bg-logo-primary hover:bg-logo-primary/90 text-white text-sm font-medium transition-colors"
                  >
                    {isWindows
                      ? t("accessibility.openSettings")
                      : t("onboarding.permissions.grant")}
                  </button>
                )}
              </div>
            </div>
          </div>
        )}

        {/* Accessibility Permission Card */}
        {showAccessibilityPermission && (
          <div className="w-full p-4 rounded-lg bg-white/5 border border-mid-gray/20">
            <div className="flex items-center gap-4">
              <div className="p-3 rounded-full bg-logo-primary/20 shrink-0">
                <Keyboard className="w-6 h-6 text-logo-primary" />
              </div>
              <div className="flex-1 min-w-0">
                <h3 className="font-medium text-text">
                  {t("onboarding.permissions.accessibility.title")}
                </h3>
                <p className="text-sm text-text/60 mb-3">
                  {t("onboarding.permissions.accessibility.description")}
                </p>
                {permissions.accessibility === "granted" ? (
                  <div className="flex items-center gap-2 text-emerald-400 text-sm">
                    <Check className="w-4 h-4" />
                    {t("onboarding.permissions.granted")}
                  </div>
                ) : permissions.accessibility === "waiting" ? (
                  <div className="flex items-center gap-2 text-text/50 text-sm">
                    <Loader2 className="w-4 h-4 animate-spin" />
                    {t("onboarding.permissions.waiting")}
                  </div>
                ) : (
                  <button
                    onClick={handleGrantAccessibility}
                    className="px-4 py-2 rounded-lg bg-logo-primary hover:bg-logo-primary/90 text-white text-sm font-medium transition-colors"
                  >
                    {t("onboarding.permissions.grant")}
                  </button>
                )}
              </div>
            </div>
          </div>
        )}

        {isMacOS &&
          accessibilityRequired &&
          permissions.microphone === "granted" &&
          permissions.accessibility !== "granted" && (
            <div className="w-full flex flex-col items-center gap-2 pt-1">
              {!showMinimumPermissionsWarning ? (
                <>
                  <p className="text-sm text-text/60 text-center">
                    {t("onboarding.permissions.minimumModeDescription")}
                  </p>
                  <button
                    onClick={() => setShowMinimumPermissionsWarning(true)}
                    className="px-4 py-2 rounded-lg border border-mid-gray/40 hover:border-logo-primary text-sm font-medium transition-colors"
                  >
                    {t("onboarding.permissions.useMinimumPermissions")}
                  </button>
                </>
              ) : (
                <div
                  role="alert"
                  className="w-full p-4 rounded-lg border border-amber-500/40 bg-amber-500/10"
                >
                  <div className="flex items-start gap-3">
                    <AlertTriangle
                      className="w-5 h-5 mt-0.5 text-amber-400 shrink-0"
                      aria-hidden="true"
                    />
                    <div className="flex-1">
                      <h3 className="font-medium text-text">
                        {t("onboarding.permissions.minimumModeWarningTitle")}
                      </h3>
                      <p className="mt-1 text-sm text-text/70">
                        {t(
                          "onboarding.permissions.minimumModeWarningDescription",
                        )}
                      </p>
                      <div className="mt-4 flex justify-end gap-2">
                        <button
                          onClick={() =>
                            setShowMinimumPermissionsWarning(false)
                          }
                          disabled={isApplyingMinimumPermissions}
                          className="px-3 py-2 rounded-lg text-sm font-medium hover:bg-white/5 transition-colors disabled:opacity-50"
                        >
                          {t("onboarding.cancel")}
                        </button>
                        <button
                          onClick={handleUseMinimumPermissions}
                          disabled={isApplyingMinimumPermissions}
                          className="px-3 py-2 rounded-lg border border-amber-500/50 hover:bg-amber-500/10 text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                        >
                          {isApplyingMinimumPermissions && (
                            <Loader2 className="inline-block w-4 h-4 mr-2 animate-spin" />
                          )}
                          {t("onboarding.permissions.useMinimumPermissions")}
                        </button>
                      </div>
                    </div>
                  </div>
                </div>
              )}
            </div>
          )}
      </div>
    </div>
  );
};

export default AccessibilityOnboarding;
