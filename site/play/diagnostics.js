export function formatDiagnostics(diagnostics) {
  return diagnostics.map(({ code, severity, message, span, notes = [], help }) => {
    const location = span ? `${span.file}:${span.startLine}:${span.startColumn}: ` : '';
    return `${location}${severity}[${code}]: ${message}`
      + notes.map(note => `\n  ${note}`).join('')
      + (help ? `\n  help: ${help}` : '');
  }).join('\n');
}
