import type { Diagnostic } from "./types.js";

/** Thrown when the compiler rejects its input. */
export class CompileError extends Error {
  override readonly name = "CompileError";
  readonly diagnostics: readonly Diagnostic[];

  constructor(diagnostics: readonly Diagnostic[]) {
    super(formatDiagnostics(diagnostics));
    this.diagnostics = diagnostics;
  }
}

/** Render diagnostics as `file:line:column: severity[code]: message` lines. */
export function formatDiagnostics(diagnostics: readonly Diagnostic[]): string {
  return diagnostics
    .map(({ code, severity, message, span, notes, help }) => {
      const location = span ? `${span.file}:${span.startLine}:${span.startColumn}: ` : "";
      const details = notes.map((note) => `\n  ${note}`).join("");
      return `${location}${severity}[${code}]: ${message}${details}${help ? `\n  help: ${help}` : ""}`;
    })
    .join("\n");
}
