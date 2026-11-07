// ANCHOR: compile
import { writeFile } from "node:fs/promises";
import { compile, CompileError } from "perry-wit";

const wit = `
package example:greeter@0.1.0;

world greeter {
  export greet: func(name: string) -> string;
}
`;

const source = `
export function greet(name: string): string {
  return \`Hello, \${name}!\`;
}
`;

try {
  const { component } = await compile({ source, wit });
  await writeFile("greeter.wasm", component);
  console.log(`greeter.wasm: ${component.byteLength} bytes`);
} catch (error) {
  if (!(error instanceof CompileError)) throw error;
  for (const { severity, code, message, span } of error.diagnostics) {
    console.error(`${span?.file}:${span?.startLine}: ${severity}[${code}] ${message}`);
  }
  process.exitCode = 1;
}
// ANCHOR_END: compile
