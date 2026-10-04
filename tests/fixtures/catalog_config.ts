import { get as configGet } from "wasi:config/store@0.2.0-rc.1";

export function read(key: string): string {
  const result = configGet(key);
  if (!result.ok) {
    const error = result.error;
    if (error.tag === "upstream") { return "upstream: " + error.val; }
    return "io: " + error.val;
  }
  const value = result.value;
  if (value === null || value === undefined) { return "visual"; }
  return value;
}
