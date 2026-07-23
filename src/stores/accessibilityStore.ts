import { type } from "@tauri-apps/plugin-os";
import { checkAccessibilityPermission } from "tauri-plugin-macos-permissions-api";
import { create } from "zustand";
import type { AccessibilityPermissionStatus } from "@/lib/accessibility";

interface AccessibilityStore {
  osType: string;
  status: AccessibilityPermissionStatus;
  refreshPermission: () => Promise<boolean>;
  setPermissionGranted: (granted: boolean) => void;
}

const osType = type();

export const useAccessibilityStore = create<AccessibilityStore>()((set) => ({
  osType,
  status: osType === "macos" ? "checking" : "not_applicable",

  refreshPermission: async () => {
    if (osType !== "macos") {
      set({ status: "not_applicable" });
      return true;
    }

    try {
      const granted = await checkAccessibilityPermission();
      set({ status: granted ? "granted" : "denied" });
      return granted;
    } catch (error) {
      set({ status: "denied" });
      throw error;
    }
  },

  setPermissionGranted: (granted) => {
    set({
      status:
        osType === "macos"
          ? granted
            ? "granted"
            : "denied"
          : "not_applicable",
    });
  },
}));
