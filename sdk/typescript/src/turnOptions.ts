export type TurnOptions = {
  /** JSON schema describing the expected agent output. */
  outputSchema?: unknown;
  /** Explicitly allow an exactly empty final answer for this request only. */
  allowNoProse?: boolean;
  /** AbortSignal to cancel the turn. */
  signal?: AbortSignal;
};
