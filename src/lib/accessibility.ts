import type {
  AutoSubmitKey,
  KeyboardImplementation,
  PasteMethod,
} from "@/bindings";
import type { OSType } from "@/lib/utils/keyboard";

export type AccessibilityPermissionStatus =
  "checking" | "granted" | "denied" | "not_applicable";

export type AutoSubmitOption = AutoSubmitKey | "off";

let captureOwnerSequence = 0;

/** Compatibility-safe capture lease for WebKit versions before randomUUID. */
export const createShortcutCaptureOwner = (prefix: string): string => {
  captureOwnerSequence += 1;
  const random = new Uint32Array(2);
  globalThis.crypto?.getRandomValues?.(random);
  return `${prefix}:${Date.now()}:${captureOwnerSequence}:${random[0].toString(16)}${random[1].toString(16)}`;
};

const MACOS_EVENT_TAP_KEYS = new Set([
  "mediafastforward",
  "mediaplaypause",
  "mediarewind",
  "mediatracknext",
  "mediatrackprevious",
]);

export const accessibilityFeatureAvailable = (
  osType: OSType,
  permissionStatus: AccessibilityPermissionStatus,
  requiresAccessibility: boolean,
): boolean =>
  osType !== "macos" ||
  !requiresAccessibility ||
  permissionStatus === "granted";

export const pasteMethodRequiresAccessibility = (
  osType: OSType,
  pasteMethod: PasteMethod,
  autoSubmit = false,
): boolean => {
  if (osType !== "macos") return false;

  if (pasteMethod === "none") return false;
  if (pasteMethod === "external_script") return autoSubmit;
  return true;
};

export const autoSubmitOptionRequiresAccessibility = (
  osType: OSType,
  option: AutoSubmitOption,
): boolean => osType === "macos" && option !== "off";

export const keyboardImplementationRequiresAccessibility = (
  osType: OSType,
  implementation: KeyboardImplementation,
): boolean => osType === "macos" && implementation === "handy_keys";

const bindingUsesMacOSEventTap = (binding: string): boolean =>
  binding
    .split("+")
    .map((key) => key.trim().toLowerCase())
    .some((key) => MACOS_EVENT_TAP_KEYS.has(key));

export const shortcutBindingRequiresAccessibility = (
  osType: OSType,
  implementation: KeyboardImplementation,
  binding: string,
): boolean =>
  keyboardImplementationRequiresAccessibility(osType, implementation) ||
  (osType === "macos" &&
    implementation === "tauri" &&
    bindingUsesMacOSEventTap(binding));

/**
 * Tauri records candidates with the webview's key events, so its editor stays
 * available even when the current binding is a restricted media key. The
 * backend permission gate still rejects a newly entered restricted candidate.
 * HandyKeys recording itself uses a native event tap and must remain gated.
 */
export const shortcutEditorRequiresAccessibility = (
  osType: OSType,
  implementation: KeyboardImplementation,
): boolean =>
  keyboardImplementationRequiresAccessibility(osType, implementation);

interface PostProcessingToggleOptions {
  osType: OSType;
  permissionStatus: AccessibilityPermissionStatus;
  keyboardImplementation: KeyboardImplementation;
  binding: string;
  enabled: boolean;
}

/** Enabling registers the post-process shortcut; disabling only removes it. */
export const postProcessingToggleDisabled = ({
  osType,
  permissionStatus,
  keyboardImplementation,
  binding,
  enabled,
}: PostProcessingToggleOptions): boolean =>
  !enabled &&
  !accessibilityFeatureAvailable(
    osType,
    permissionStatus,
    shortcutBindingRequiresAccessibility(
      osType,
      keyboardImplementation,
      binding,
    ),
  );

interface InputInitializationOptions {
  osType: OSType;
  pasteMethod: PasteMethod;
  autoSubmit: boolean;
  accessibilityGranted: boolean;
}

export interface InputInitializationPlan {
  shortcuts: true;
  syntheticInput: boolean;
}

/**
 * Plan the one-time native input initialization performed after onboarding.
 * Runtime setting commands initialize capabilities added later, so React does
 * not need to own or repeatedly restart those native lifecycles.
 */
export const createInputInitializationPlan = ({
  osType,
  pasteMethod,
  autoSubmit,
  accessibilityGranted,
}: InputInitializationOptions): InputInitializationPlan => ({
  shortcuts: true,
  syntheticInput:
    osType !== "macos" ||
    (accessibilityGranted &&
      pasteMethodRequiresAccessibility(osType, pasteMethod, autoSubmit)),
});
