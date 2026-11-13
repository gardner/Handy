import { test, expect, type Page } from "@playwright/test";

// Render the real permission hook and settings controls with macOS IPC mocked.
// Actual TCC revocation and event-tap teardown still require a Mac.
async function openHarness(
  page: Page,
  mode: "onboarding" | "settings",
  options: { platform?: "macos" | "linux"; microphone?: boolean } = {},
) {
  page.on("pageerror", (error) => console.error(error.message));
  await page.route("**/__accessibility_test", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: `<!doctype html><html><body><div id="root"></div>
<script>
window.accessibilityGranted = false;
window.microphoneGranted = ${options.microphone ?? true};
window.permissionRequests = [];
window.initializationError = false;
window.__TAURI_OS_PLUGIN_INTERNALS__ = { platform: "${options.platform ?? "macos"}", os_type: "${options.platform ?? "macos"}" };
window.__TAURI_INTERNALS__ = {
  metadata: { currentWindow: { label: "main" } },
  transformCallback: () => 1,
  unregisterCallback: () => {},
  invoke: async (command) => {
    window.permissionRequests.push(command);
    if (command.endsWith("check_accessibility_permission")) return window.accessibilityGranted;
    if (command.endsWith("check_microphone_permission")) return window.microphoneGranted;
    if (command === "initialize_enigo" && window.initializationError) throw "Input initialization failed";
    if (command === "get_available_microphones" || command === "get_available_output_devices") return [];
    return null;
  }
};
</script>
<script type="module">
import RefreshRuntime from '/@react-refresh';
RefreshRuntime.injectIntoGlobalHook(window);
window.$RefreshReg$ = () => {};
window.$RefreshSig$ = () => (type) => type;
window.__vite_plugin_react_preamble_installed__ = true;
</script>
<script type="module">
import React from '/node_modules/.vite/deps/react.js';
import ReactDOM from '/node_modules/.vite/deps/react-dom_client.js';
const { createRoot } = ReactDOM;
import '/src/i18n/index.ts';
import { AccessibilityProvider } from '/src/hooks/useAccessibility.tsx';
import { AccessibilityNote } from '/src/components/settings/AccessibilityNote.tsx';
import { ShortcutInput } from '/src/components/settings/ShortcutInput.tsx';
import { PasteMethodSetting } from '/src/components/settings/PasteMethod.tsx';
import { ClipboardHandlingSetting } from '/src/components/settings/ClipboardHandling.tsx';
import { ShortcutActivationSetting } from '/src/components/settings/ShortcutActivation.tsx';
import { AutoSubmit } from '/src/components/settings/AutoSubmit.tsx';
import { useSettingsStore } from '/src/stores/settingsStore.ts';
import { Permissions } from '/src/components/onboarding/screens/Permissions.tsx';
import { usePermissions } from '/src/components/onboarding/usePermissions.ts';
const settings = {
  bindings: { transcribe: { id: "transcribe", name: "Transcribe", description: "", current_binding: "option+space" } },
  paste_method: "ctrl_v", auto_submit: true, auto_submit_key: "enter",
  clipboard_handling: "dont_modify", shortcut_activation: "toggle"
};
useSettingsStore.setState({ settings, isLoading: false });
function Onboarding() {
  const permissions = usePermissions(false);
  return React.createElement(Permissions, { permissions, onContinue: () => { document.body.dataset.continued = "true"; } });
}
function Settings() {
  return React.createElement(React.Fragment, null,
    React.createElement(AccessibilityNote),
    React.createElement(ShortcutInput, { shortcutId: "transcribe" }),
    React.createElement(PasteMethodSetting),
    React.createElement(AutoSubmit),
    React.createElement(ClipboardHandlingSetting),
    React.createElement(ShortcutActivationSetting));
}
createRoot(document.getElementById('root')).render(
  React.createElement(AccessibilityProvider, null, React.createElement(${mode === "onboarding" ? "Onboarding" : "Settings"})));
</script></body></html>`,
    }),
  );
  await page.goto("/__accessibility_test");
}

test("macOS setup can continue with microphone access only", async ({
  page,
}) => {
  await openHarness(page, "onboarding");
  await expect(
    page.getByText(/Optional: enables global shortcuts/),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Continue", exact: true }),
  ).toBeEnabled();
  await page.getByRole("button", { name: "Continue", exact: true }).click();
  await expect(page.locator("body")).toHaveAttribute("data-continued", "true");
  const requests = await page.evaluate(
    () => Reflect.get(window, "permissionRequests") as string[],
  );
  expect(
    requests.some((command) => command.includes("request_accessibility")),
  ).toBe(false);
});

test("keyboard settings follow permission changes and retain saved preferences", async ({
  page,
}) => {
  await openHarness(page, "settings");
  // The shortcut text uses the app's key formatter; target its known saved binding.
  const shortcutButton = page.locator("button").filter({ hasText: /Space/ });
  await expect(shortcutButton).toBeDisabled();
  await expect(page.getByText("Accessibility Is Optional")).toBeVisible();
  await expect(
    page.getByText(/System Settings → Privacy & Security → Accessibility/),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Enter", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "Don't Modify Clipboard", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "Toggle", exact: true }),
  ).toBeDisabled();
  await expect(
    page.locator('button[aria-haspopup="listbox"]:enabled'),
  ).toHaveCount(0);
  await page.evaluate(() => Reflect.set(window, "accessibilityGranted", true));
  await expect(shortcutButton).toBeEnabled();
  await expect(page.getByText("Accessibility Is Optional")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Enter", exact: true }),
  ).toBeEnabled();
  await page.evaluate(() => Reflect.set(window, "accessibilityGranted", false));
  await expect(shortcutButton).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "Enter", exact: true }),
  ).toBeDisabled();
});

test("microphone permission is still required for setup", async ({ page }) => {
  await openHarness(page, "onboarding", { microphone: false });
  await expect(
    page.getByRole("button", { name: "Continue", exact: true }),
  ).toBeDisabled();
});

test("the settings note opens the Accessibility permission request only on click", async ({
  page,
}) => {
  await openHarness(page, "settings");
  await page
    .getByRole("button", { name: "Open settings", exact: true })
    .click();
  await expect
    .poll(() =>
      page.evaluate(
        () => Reflect.get(window, "permissionRequests") as string[],
      ),
    )
    .toContain("reset_macos_permission");
  await expect
    .poll(() =>
      page.evaluate(
        () => Reflect.get(window, "permissionRequests") as string[],
      ),
    )
    .toContain("plugin:macos-permissions|request_accessibility_permission");
});

test("non-macOS settings do not depend on Accessibility", async ({ page }) => {
  await openHarness(page, "settings", { platform: "linux" });
  await expect(
    page.locator("button").filter({ hasText: /Space/ }),
  ).toBeEnabled();
  await expect(page.getByText("Accessibility Is Optional")).toHaveCount(0);
});

test("input initialization retries after a command error", async ({ page }) => {
  await openHarness(page, "settings");
  await page.evaluate(() => {
    Reflect.set(window, "initializationError", true);
    Reflect.set(window, "accessibilityGranted", true);
  });
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          (Reflect.get(window, "permissionRequests") as string[]).filter(
            (command) => command === "initialize_enigo",
          ).length,
      ),
    )
    .toBeGreaterThanOrEqual(2);
  await page.evaluate(() => Reflect.set(window, "initializationError", false));
  await expect(
    page.locator("button").filter({ hasText: /Space/ }),
  ).toBeEnabled();
});
