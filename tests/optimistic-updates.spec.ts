import { expect, test } from "@playwright/test";
import {
  attemptCommandRecovery,
  persistOptimisticUpdate,
} from "../src/lib/commandResult";

test.describe("optimistic setting updates", () => {
  test("accepts both generated Result values and commands returning void", async () => {
    const values: string[] = [];

    await persistOptimisticUpdate({
      apply: () => values.push("optimistic"),
      persist: async () => ({ status: "ok", data: null }),
      rollback: () => values.push("rollback"),
      operation: "update setting",
    });
    await persistOptimisticUpdate({
      apply: () => values.push("void command"),
      persist: async () => undefined,
      rollback: () => values.push("rollback"),
      operation: "update setting",
    });

    expect(values).toEqual(["optimistic", "void command"]);
  });

  test("treats a resolved generated error as failure and rolls back", async () => {
    const values: string[] = [];

    await expect(
      persistOptimisticUpdate({
        apply: () => values.push("optimistic"),
        persist: async () => ({
          status: "error",
          error: "Accessibility permission is required",
        }),
        rollback: () => values.push("rollback"),
        operation: "update paste method",
      }),
    ).rejects.toThrow(
      "update paste method: Accessibility permission is required",
    );
    expect(values).toEqual(["optimistic", "rollback"]);
  });

  test("also rolls back commands that reject their promise", async () => {
    const values: string[] = [];

    await expect(
      persistOptimisticUpdate({
        apply: () => values.push("optimistic"),
        persist: async () => {
          throw new Error("transport failed");
        },
        rollback: () => values.push("rollback"),
        operation: "update setting",
      }),
    ).rejects.toThrow("transport failed");
    expect(values).toEqual(["optimistic", "rollback"]);
  });

  test("reports successful command recovery", async () => {
    await expect(
      attemptCommandRecovery(async () => undefined),
    ).resolves.toEqual({ recovered: true });
  });

  test("retains the recovery error for a retryable UI state", async () => {
    const recoveryError = new Error("shortcut restore failed");

    const result = await attemptCommandRecovery(async () => {
      throw recoveryError;
    });

    expect(result).toEqual({ recovered: false, error: recoveryError });
  });
});
