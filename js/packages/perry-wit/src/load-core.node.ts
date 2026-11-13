import { readFile } from "node:fs/promises";
import { coreModules } from "../bindings/core-modules.js";

export async function loadCoreModule(path: string): Promise<WebAssembly.Module> {
  const url = coreModules[path];
  if (!url) throw new Error(`perry-wit: missing compiler module ${path}`);
  return WebAssembly.compile(await readFile(url));
}
