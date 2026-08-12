type GeneratedCommandResult =
  { status: "ok"; data: unknown } | { status: "error"; error: unknown };

const isGeneratedCommandResult = (
  value: unknown,
): value is GeneratedCommandResult => {
  if (typeof value !== "object" || value === null || !("status" in value)) {
    return false;
  }

  return value.status === "ok" || value.status === "error";
};

export const assertCommandSucceeded = (
  result: unknown,
  operation: string,
): void => {
  if (isGeneratedCommandResult(result) && result.status === "error") {
    throw new Error(`${operation}: ${String(result.error)}`);
  }
};

export type CommandRecoveryResult =
  { recovered: true } | { recovered: false; error: unknown };

/**
 * Run best-effort command cleanup without losing the failure that tells a UI
 * to keep its retry control visible.
 */
export const attemptCommandRecovery = async (
  recover: () => Promise<void>,
): Promise<CommandRecoveryResult> => {
  try {
    await recover();
    return { recovered: true };
  } catch (error) {
    return { recovered: false, error };
  }
};

interface OptimisticUpdateOptions {
  apply: () => void;
  persist: () => Promise<unknown>;
  rollback: () => void;
  operation: string;
}

/**
 * Persist an optimistic update while handling both Tauri command shapes used
 * in the generated bindings: rejected promises and resolved `Result::error`s.
 */
export const persistOptimisticUpdate = async ({
  apply,
  persist,
  rollback,
  operation,
}: OptimisticUpdateOptions): Promise<void> => {
  apply();

  try {
    assertCommandSucceeded(await persist(), operation);
  } catch (error) {
    rollback();
    throw error;
  }
};
