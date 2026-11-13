import { coreModules } from "../bindings/core-modules.js";

export async function loadCoreModule(path: string): Promise<WebAssembly.Module> {
  const url = coreModules[path];
  if (!url) throw new Error(`perry-wit: missing compiler module ${path}`);
  const response = await fetch(url);
  if (!response.ok) throw new Error(`perry-wit: could not load ${url}: HTTP ${response.status}`);
  // Streaming compilation needs an application/wasm response, which not every server sends.
  if (response.headers.get("content-type")?.startsWith("application/wasm")) {
    return WebAssembly.compileStreaming(response);
  }
  return WebAssembly.compile(await response.arrayBuffer());
}
