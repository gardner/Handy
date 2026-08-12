import React from "react";
import { useTranslation } from "react-i18next";
import { SettingContainer } from "../../ui/SettingContainer";
import { Dropdown, type DropdownOption } from "../../ui/Dropdown";
import { useSettings } from "../../../hooks/useSettings";
import { commands } from "@/bindings";
import { toast } from "sonner";
import { useOsType } from "@/hooks/useOsType";
import {
  accessibilityFeatureAvailable,
  keyboardImplementationRequiresAccessibility,
} from "@/lib/accessibility";
import { useAccessibilityStore } from "@/stores/accessibilityStore";
import { AccessibilityRequiredNote } from "../AccessibilityRequiredNote";

interface KeyboardImplementationSelectorProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const KeyboardImplementationSelector: React.FC<
  KeyboardImplementationSelectorProps
> = ({ descriptionMode = "tooltip", grouped = false }) => {
  const { t } = useTranslation();
  const osType = useOsType();
  const { getSetting, isUpdating, refreshSettings } = useSettings();
  const accessibilityStatus = useAccessibilityStore((state) => state.status);
  const [isChangingImplementation, setIsChangingImplementation] =
    React.useState(false);
  const currentImplementation =
    getSetting("keyboard_implementation") ?? "tauri";
  const keyboardImplementationOptions: DropdownOption[] = [
    {
      value: "tauri",
      label: "Tauri Global Shortcut",
    },
    {
      value: "handy_keys",
      label: "Handy Keys",
      disabled: !accessibilityFeatureAvailable(
        osType,
        accessibilityStatus,
        keyboardImplementationRequiresAccessibility(osType, "handy_keys"),
      ),
    },
  ];

  const handleSelect = async (value: string) => {
    if (value === currentImplementation || isChangingImplementation) return;

    setIsChangingImplementation(true);
    try {
      const result = await commands.changeKeyboardImplementationSetting(value);

      if (result.status === "error") {
        console.error(
          "Failed to update keyboard implementation:",
          result.error,
        );
        toast.error(String(result.error));
        return;
      }

      // If any bindings were reset due to incompatibility, notify the user
      if (result.data.reset_bindings.length > 0) {
        toast.warning(t("settings.debug.keyboardImplementation.bindingsReset"));
      }

      await refreshSettings();
    } catch (error) {
      console.error("Failed to update keyboard implementation:", error);
      toast.error(String(error));
    } finally {
      setIsChangingImplementation(false);
    }
  };

  return (
    <SettingContainer
      title={t("settings.debug.keyboardImplementation.title")}
      description={t("settings.debug.keyboardImplementation.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
      layout="horizontal"
    >
      <div className="flex flex-col gap-2">
        <Dropdown
          options={keyboardImplementationOptions}
          selectedValue={currentImplementation}
          onSelect={handleSelect}
          disabled={
            isChangingImplementation || isUpdating("keyboard_implementation")
          }
        />
        {osType === "macos" && accessibilityStatus === "denied" && (
          <AccessibilityRequiredNote />
        )}
      </div>
    </SettingContainer>
  );
};
