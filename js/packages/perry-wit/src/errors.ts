import type { Diagnostic, SourceSpan } from "./types.js";

/** Thrown when the compiler rejects its input. */
export class CompileError extends Error {
  override readonly name = "CompileError";
  readonly diagnostics: readonly Diagnostic[];

  constructor(diagnostics: readonly Diagnostic[]) {
    if (diagnostics.length === 0) {
      throw new RangeError("CompileError requires at least one diagnostic");
    }
    const snapshot = Object.freeze([...diagnostics]);
    super(formatDiagnostics(snapshot));
    this.diagnostics = snapshot;
  }

  get primary(): Diagnostic {
    return this.diagnostics[0]!;
  }
}

function formatLocation(span?: SourceSpan): string {
  return span ? `${span.file}:${span.startLine}:${span.startColumn}: ` : "";
}

/** Render diagnostics as `file:line:column: error[code]: message` lines. */
export function formatDiagnostics(diagnostics: readonly Diagnostic[]): string {
  return diagnostics
    .map(({ code, message, span, notes, help }) => {
      const location = formatLocation(span);
      const details = notes.map((note) => `\n  ${formatLocation(note.span)}${note.message}`).join("");
      return `${location}error[${code}]: ${message}${details}${help ? `\n  help: ${help}` : ""}`;
    })
    .join("\n");
}
