import React, { useEffect, useState, useRef, useCallback } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { formatKeyCombination } from "../../lib/utils/keyboard";
import { ResetButton } from "../ui/ResetButton";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import { useOsType } from "../../hooks/useOsType";
import { commands } from "@/bindings";
import { toast } from "sonner";
import { openUrl } from "@tauri-apps/plugin-opener";
import { SECURE_INPUT_HELP_URL } from "../SecureInputWarning";
import { unwrapCommandResult } from "@/lib/permissions";
import { attemptCommandRecovery } from "@/lib/commandResult";

interface HandyKeysShortcutInputProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
  shortcutId: string;
  disabled?: boolean;
}

interface HandyKeysEvent {
  modifiers: string[];
  key: string | null;
  is_key_down: boolean;
  hotkey_string: string;
}

export const HandyKeysShortcutInput: React.FC<HandyKeysShortcutInputProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
  shortcutId,
  disabled = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateBinding, resetBinding, isUpdating, isLoading } =
    useSettings();
  const [isRecording, setIsRecording] = useState(false);
  const [currentKeys, setCurrentKeys] = useState<string>("");
  const [originalBinding, setOriginalBinding] = useState<string>("");
  const shortcutRef = useRef<HTMLDivElement | null>(null);
  const unlistenRef = useRef<(() => void) | null>(null);
  const isRecordingRef = useRef(false);
  // Use a ref to track currentKeys for the event handler (avoids stale closure)
  const currentKeysRef = useRef<string>("");
  // Track keyed vs modifier-only captures separately so a combo commits only
  // on its key's release and a modifier-only shortcut only once every
  // modifier is released. Committing on the *first* release (the old
  // behavior) silently saved just the modifier whenever the key event never
  // arrived — e.g. while macOS Secure Input is active (issue #1578).
  const keyedShortcutRef = useRef<string>("");
  const modifierOnlyShortcutRef = useRef<string>("");
  const osType = useOsType();

  const bindings = getSetting("bindings") || {};

  const clearRecordingState = useCallback(() => {
    isRecordingRef.current = false;
    setIsRecording(false);
    setCurrentKeys("");
    currentKeysRef.current = "";
    keyedShortcutRef.current = "";
    modifierOnlyShortcutRef.current = "";
    setOriginalBinding("");
  }, []);

  const stopBackendRecording = useCallback(async () => {
    unwrapCommandResult(
      await commands.stopHandyKeysRecording(),
      "Stop shortcut recording",
    );
  }, []);

  // Handle cancellation
  const cancelRecording = useCallback(async () => {
    if (!isRecording) return;

    // Stop listening for backend events
    if (unlistenRef.current) {
      unlistenRef.current();
      unlistenRef.current = null;
    }

    // Keep the editor active when backend cleanup fails; clearing it would
    // hide a still-live recorder or still-suspended shortcuts.
    try {
      await stopBackendRecording();
    } catch (error) {
      console.error("Failed to stop shortcut recording:", error);
      toast.error(t("settings.general.shortcut.errors.restore"));
      return;
    }

    // Restore original binding
    if (originalBinding) {
      try {
        await updateBinding(shortcutId, originalBinding);
      } catch (error) {
        console.error("Failed to restore original binding:", error);
        toast.error(t("settings.general.shortcut.errors.restore"));
      }
    }

    clearRecordingState();
  }, [
    isRecording,
    originalBinding,
    shortcutId,
    updateBinding,
    t,
    stopBackendRecording,
    clearRecordingState,
  ]);

  // Listener failure can happen without a final key event. Keep this listener
  // mounted so an immediate backend failure cannot race React's recording
  // state update and leave the editor stuck open.
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;

    void listen<string>("handy-keys-recording-stopped", (event) => {
      if (disposed || !isRecordingRef.current) return;

      unlistenRef.current?.();
      unlistenRef.current = null;
      clearRecordingState();
      toast.error(
        t("settings.general.shortcut.errors.set", {
          error: event.payload,
        }),
      );
    })
      .then((dispose) => {
        if (disposed) {
          dispose();
        } else {
          unlisten = dispose;
        }
      })
      .catch(console.error);

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [clearRecordingState, t]);

  // Set up event listener for handy-keys events
  useEffect(() => {
    if (!isRecording) return;

    let cleanup = false;

    const setupListener = async () => {
      // Listen for key events from backend
      const commitAndStop = async (keysToCommit: string) => {
        try {
          await updateBinding(shortcutId, keysToCommit);
        } catch (error) {
          console.error("Failed to change binding:", error);
          toast.error(
            t("settings.general.shortcut.errors.set", {
              error: String(error),
            }),
          );

          // Reset to original binding on error
          if (originalBinding) {
            try {
              await updateBinding(shortcutId, originalBinding);
            } catch (resetError) {
              console.error("Failed to reset binding:", resetError);
              toast.error(t("settings.general.shortcut.errors.reset"));
            }
          }
        }

        // Stop recording
        if (unlistenRef.current) {
          unlistenRef.current();
          unlistenRef.current = null;
        }
        try {
          await stopBackendRecording();
          clearRecordingState();
        } catch (error) {
          console.error("Failed to stop shortcut recording:", error);
          toast.error(t("settings.general.shortcut.errors.restore"));
        }
      };

      const unlisten = await listen<HandyKeysEvent>(
        "handy-keys-event",
        async (event) => {
          if (cleanup) return;

          const { hotkey_string, is_key_down, key, modifiers } = event.payload;

          if (is_key_down && hotkey_string) {
            // Update both state (for display) and refs (for release handler)
            if (key) {
              keyedShortcutRef.current = hotkey_string;
            } else {
              modifierOnlyShortcutRef.current = hotkey_string;
            }
            currentKeysRef.current = hotkey_string;
            setCurrentKeys(hotkey_string);
          } else if (!is_key_down && key) {
            // The main key was released — commit the keyed combo. The release
            // event's hotkey_string still contains the key, so it works even
            // if the key-down was somehow missed. Never fall back to a
            // modifier-only capture here: that's how bindings used to get
            // silently overwritten with just the modifier (issue #1578).
            const keysToCommit = keyedShortcutRef.current || hotkey_string;
            if (keysToCommit) {
              await commitAndStop(keysToCommit);
            }
          } else if (
            !is_key_down &&
            !key &&
            modifiers.length === 0 &&
            !keyedShortcutRef.current &&
            modifierOnlyShortcutRef.current
          ) {
            // Every modifier released without a main key ever going down —
            // commit as a modifier-only shortcut
            await commitAndStop(modifierOnlyShortcutRef.current);
          }
        },
      );

      unlistenRef.current = unlisten;
    };

    void setupListener().catch(async (error) => {
      console.error("Failed to listen for shortcut events:", error);
      try {
        await stopBackendRecording();
        clearRecordingState();
      } catch (stopError) {
        console.error("Failed to stop shortcut recording:", stopError);
      }
      toast.error(t("settings.general.shortcut.errors.restore"));
    });

    return () => {
      cleanup = true;
      if (unlistenRef.current) {
        unlistenRef.current();
        unlistenRef.current = null;
      }
      // Stop backend recording on unmount to prevent orphaned recording loops
      if (isRecordingRef.current) {
        void stopBackendRecording().catch(console.error);
      }
    };
  }, [
    isRecording,
    shortcutId,
    originalBinding,
    updateBinding,
    cancelRecording,
    t,
    stopBackendRecording,
    clearRecordingState,
  ]);

  // Handle click outside
  useEffect(() => {
    if (!isRecording) return;

    const handleClickOutside = (e: MouseEvent) => {
      if (
        shortcutRef.current &&
        !shortcutRef.current.contains(e.target as Node)
      ) {
        cancelRecording();
      }
    };

    window.addEventListener("click", handleClickOutside);
    return () => window.removeEventListener("click", handleClickOutside);
  }, [isRecording, cancelRecording]);

  // Start recording a new shortcut
  const startRecording = async () => {
    if (disabled || isRecording) return;

    // Store the original binding to restore if canceled
    setOriginalBinding(bindings[shortcutId]?.current_binding || "");
    isRecordingRef.current = true;

    const recoverFailedStart = async () => {
      const recovery = await attemptCommandRecovery(stopBackendRecording);
      if (recovery.recovered) {
        clearRecordingState();
        return;
      }

      console.error(
        "Failed to restore shortcuts after recording startup:",
        recovery.error,
      );
      // The backend retains the capture lease when restoration fails. Keep a
      // visible capture state so click-outside can retry cleanup instead of
      // leaving every static shortcut silently suspended.
      setIsRecording(true);
      setCurrentKeys("");
      currentKeysRef.current = "";
      keyedShortcutRef.current = "";
      modifierOnlyShortcutRef.current = "";
      toast.error(t("settings.general.shortcut.errors.restore"));
    };

    // Start backend recording. The backend refuses while macOS Secure Input
    // is active (the recorder's listener would receive no key events and
    // capture just the modifier) — it also flips the warning banner on, so
    // the toast points at a visible explanation.
    try {
      const result = await commands.startHandyKeysRecording(shortcutId);
      if (result.status === "error") {
        await recoverFailedStart();
        if (String(result.error).includes("secure-input-active")) {
          toast.error(t("secureInput.recorderBlocked"), {
            action: {
              label: t("secureInput.learnMore"),
              onClick: () => openUrl(SECURE_INPUT_HELP_URL),
            },
          });
        } else {
          toast.error(
            t("settings.general.shortcut.errors.set", {
              error: String(result.error),
            }),
          );
        }
        return;
      }
      if (!isRecordingRef.current) {
        // A terminal-listener event won the race with this command response.
        return;
      }
      setIsRecording(true);
      setCurrentKeys("");
      currentKeysRef.current = "";
      keyedShortcutRef.current = "";
      modifierOnlyShortcutRef.current = "";
    } catch (error) {
      await recoverFailedStart();
      console.error("Failed to start recording:", error);
      toast.error(
        t("settings.general.shortcut.errors.set", { error: String(error) }),
      );
    }
  };

  // Format the current shortcut keys being recorded
  const formatCurrentKeys = (): string => {
    if (!currentKeys) return t("settings.general.shortcut.pressKeys");
    return formatKeyCombination(currentKeys, osType);
  };

  // If still loading, show loading state
  if (isLoading) {
    return (
      <SettingContainer
        title={t("settings.general.shortcut.title")}
        description={t("settings.general.shortcut.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="text-sm text-mid-gray">
          {t("settings.general.shortcut.loading")}
        </div>
      </SettingContainer>
    );
  }

  // If no bindings are loaded, show empty state
  if (Object.keys(bindings).length === 0) {
    return (
      <SettingContainer
        title={t("settings.general.shortcut.title")}
        description={t("settings.general.shortcut.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="text-sm text-mid-gray">
          {t("settings.general.shortcut.none")}
        </div>
      </SettingContainer>
    );
  }

  const binding = bindings[shortcutId];
  if (!binding) {
    return (
      <SettingContainer
        title={t("settings.general.shortcut.title")}
        description={t("settings.general.shortcut.notFound")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="text-sm text-mid-gray">
          {t("settings.general.shortcut.none")}
        </div>
      </SettingContainer>
    );
  }

  // Get translated name and description for the binding
  const translatedName = t(
    `settings.general.shortcut.bindings.${shortcutId}.name`,
    binding.name,
  );
  const translatedDescription = t(
    `settings.general.shortcut.bindings.${shortcutId}.description`,
    binding.description,
  );

  return (
    <SettingContainer
      title={translatedName}
      description={translatedDescription}
      descriptionMode={descriptionMode}
      grouped={grouped}
      disabled={disabled}
      layout="horizontal"
    >
      <div className="flex items-center space-x-1">
        {isRecording ? (
          <div
            ref={shortcutRef}
            className="px-2 py-1 text-sm font-semibold border border-logo-primary bg-logo-primary/30 rounded-md"
          >
            {formatCurrentKeys()}
          </div>
        ) : (
          <div
            className={`px-2 py-1 text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 rounded-md ${
              disabled
                ? "opacity-50 cursor-not-allowed"
                : "hover:bg-logo-primary/10 cursor-pointer hover:border-logo-primary"
            }`}
            onClick={disabled ? undefined : startRecording}
            aria-disabled={disabled}
          >
            {formatKeyCombination(binding.current_binding, osType)}
          </div>
        )}
        <ResetButton
          onClick={() => resetBinding(shortcutId)}
          disabled={disabled || isUpdating(`binding_${shortcutId}`)}
        />
      </div>
    </SettingContainer>
  );
};
