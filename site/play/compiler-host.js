import * as clocks from '@bytecodealliance/preview2-shim/clocks';
import * as random from '@bytecodealliance/preview2-shim/random';
import * as io from '@bytecodealliance/preview2-shim/io';
import * as cli from '@bytecodealliance/preview2-shim/cli';
import * as filesystem from '@bytecodealliance/preview2-shim/filesystem';

const camel = name => name.replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());

/** Standard WASI interfaces with no filesystem grants. */
export function compilerHost(names) {
  const groups = { 'wasi:clocks': clocks, 'wasi:random': random, 'wasi:io': io, 'wasi:cli': cli,
    'wasi:filesystem': { ...filesystem, preopens: { getDirectories: () => [] } } };
  return Object.fromEntries(names.map(name => {
    const [group, version] = name.split('@');
    if (version && !version.startsWith('0.2.')) throw new Error(`Unexpected compiler import: ${name}`);
    const [namespace, member] = group.split('/');
    const value = groups[namespace]?.[camel(member)];
    if (!value) throw new Error(`Unsupported compiler import: ${name}`);
    return [name, value];
  }));
}
