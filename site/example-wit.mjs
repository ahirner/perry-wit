/** Reduce the checked-in example package to one world and its local interfaces. */
export function exampleWit(source, world) {
  const text = source.replace(/\/\*[\s\S]*?\*\/|\/\/[^\n]*/g, '');
  const packageName = text.match(/\bpackage\s+([^;]+);/)?.[1];
  const declarations = new Map();
  for (const match of text.matchAll(/\b(world|interface)\s+([\w-]+)\s*\{/g)) {
    let end = match.index + match[0].length, depth = 1;
    const start = end;
    while (depth && end < text.length) {
      if (text[end] === '{') depth++;
      if (text[end] === '}') depth--;
      end++;
    }
    if (depth) throw new Error(`Unclosed example WIT declaration: ${match[2]}`);
    declarations.set(match[2], { kind: match[1], body: text.slice(start, end - 1).trim() });
  }
  function expand(name, ancestors = []) {
    const declaration = declarations.get(name);
    if (!declaration || declaration.kind !== 'world' || ancestors.includes(name)) throw new Error(`Invalid example world: ${name}`);
    return declaration.body.replace(/\binclude\s+([\w-]+)\s*;/g, (_, included) => expand(included, [...ancestors, name]));
  }
  const body = expand(world);
  const interfaces = new Map();
  function dependencies(body) {
    for (const match of body.matchAll(/\b(?:use|import|export)\s+([\w-]+)\s*[.;]/g)) {
      const name = match[1], declaration = declarations.get(name);
      if (declaration?.kind === 'interface' && !interfaces.has(name)) {
        interfaces.set(name, declaration.body);
        dependencies(declaration.body);
      }
    }
  }
  dependencies(body);
  if (!packageName) throw new Error('Example WIT needs a package declaration');
  const indent = body => body.split('\n').map(line => `  ${line.trimEnd().replace(/^  /, '')}`).join('\n');
  return `package ${packageName};\n\n${[...interfaces].map(([name, body]) => `interface ${name} {\n${indent(body)}\n}\n\n`).join('')}world ${world} {\n${indent(body)}\n}\n`;
}
