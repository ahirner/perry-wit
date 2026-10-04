import { lookup } from "test:lookup/service";

export function read(key: string): string {
  const result = lookup(key);
  if (!result.ok) {
    const failure = result.error;
    if (failure.tag === "denied") { return "denied"; }
    return "offline: " + failure.val;
  }
  const value = result.value;
  if (value === null || value === undefined) { return "fallback"; }
  return value;
}
