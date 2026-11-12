import {
  createContext,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from "react";
import { platform } from "@tauri-apps/plugin-os";
import { checkAccessibilityPermission } from "tauri-plugin-macos-permissions-api";
import { commands } from "@/bindings";

const AccessibilityContext = createContext(false);

/** One live permission check shared by every settings control. */
export function AccessibilityProvider({ children }: { children: ReactNode }) {
  const isMac = platform() === "macos";
  const [granted, setGranted] = useState(!isMac);
  useEffect(() => {
    if (!isMac) return;
    let disposed = false;
    let checking = false;
    let previous = false;
    const check = async () => {
      if (checking) return;
      checking = true;
      try {
        const allowed = await checkAccessibilityPermission();
        if (disposed) return;
        setGranted(allowed);
        if (allowed && !previous) await commands.initializeEnigo();
        previous = allowed;
      } catch (error) {
        console.warn("Failed to check Accessibility access:", error);
        if (!disposed) setGranted(false);
      } finally {
        checking = false;
      }
    };
    void check();
    const timer = setInterval(check, 1000);
    window.addEventListener("focus", check);
    return () => {
      disposed = true;
      clearInterval(timer);
      window.removeEventListener("focus", check);
    };
  }, [isMac]);
  return (
    <AccessibilityContext.Provider value={granted}>
      {children}
    </AccessibilityContext.Provider>
  );
}

export const useAccessibility = () => useContext(AccessibilityContext);
