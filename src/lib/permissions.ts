import type { Result } from "@/bindings";

interface MacOSPermissionState {
  microphoneGranted: boolean;
  accessibilityGranted: boolean;
  accessibilityRequired: boolean;
}

export const hasRequiredMacOSPermissions = ({
  microphoneGranted,
  accessibilityGranted,
  accessibilityRequired,
}: MacOSPermissionState): boolean =>
  microphoneGranted && (!accessibilityRequired || accessibilityGranted);

export const unwrapCommandResult = <T, E>(
  result: Result<T, E>,
  operation: string,
): T => {
  if (result.status === "error") {
    throw new Error(`${operation}: ${String(result.error)}`);
  }

  return result.data;
};
