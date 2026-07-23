export type AccessibilityPermissionStatus =
  "checking" | "granted" | "denied" | "not_applicable";

export const accessibilityFeatureAvailable = (
  osType: string,
  permissionStatus: AccessibilityPermissionStatus,
  requiresAccessibility: boolean,
): boolean =>
  osType !== "macos" ||
  !requiresAccessibility ||
  permissionStatus === "granted";

export const pasteMethodRequiresAccessibility = (
  osType: string,
  pasteMethod: string,
): boolean => osType === "macos" && pasteMethod !== "none";

export const autoSubmitOptionRequiresAccessibility = (
  osType: string,
  option: string,
): boolean => osType === "macos" && option !== "off";

export const keyboardImplementationRequiresAccessibility = (
  osType: string,
  implementation: string,
): boolean => osType === "macos" && implementation === "handy_keys";
