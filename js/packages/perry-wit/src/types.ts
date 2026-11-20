/** Text, or its UTF-8 encoding. */
export type FileContents = string | Uint8Array;

/** A TypeScript module or WIT file. */
export interface SourceFile {
  /**
   * Package-relative path such as `src/main.ts`.
   * Diagnostics report it, and other modules import a module by it.
   * Paths are logical names only: the compiler never reads the host filesystem.
   */
  path?: string;
  contents: FileContents;
}

/** Modules the entry module may import, keyed by package-relative path. */
export type Modules =
  | Readonly<Record<string, FileContents>>
  | ReadonlyMap<string, FileContents>;

/**
 * One WIT package, given as:
 *
 * - the text of a single WIT file,
 * - several files that together declare one package, or
 * - a binary-encoded WIT package (bytes beginning with `\0asm`).
 */
export type WitPackage = FileContents | SourceFile | readonly (FileContents | SourceFile)[];

/** The TypeScript program to compile. */
export interface ProgramOptions {
  /** The entry module. Its exports become the component's exports. */
  source: FileContents | SourceFile;
  /** Modules the entry module imports with relative specifiers. */
  modules?: Modules;
}

/** The WIT world a program implements. */
export interface WitOptions {
  /** The application's WIT package. */
  wit: WitPackage;
  /**
   * Packages the application's WIT package uses.
   * WASI 0.3 packages are built in and need not be listed.
   */
  witDependencies?: readonly WitPackage[];
  /** World to select when the application package declares more than one. */
  world?: string;
}

/** Options for {@link compile}. */
export interface CompileOptions extends ProgramOptions, WitOptions {
  /** Emit only the core WebAssembly module, which keeps the world's canonical ABI. */
  coreOnly?: boolean;
  /** Keep function names in the component's `name` section, for guest profiling. */
  keepNames?: boolean;
}

/** A WASI 0.3 WebAssembly component and the core module it wraps. */
export interface CompiledComponent {
  /** The component binary, ready to write to a `.wasm` file or run in a host. */
  component: Uint8Array<ArrayBuffer>;
  /** The core WebAssembly module inside the component. */
  core: Uint8Array<ArrayBuffer>;
  /** A human-readable summary of how the program allocates memory. */
  allocationReport: string;
}

/** A core WebAssembly module that uses its world's canonical ABI. */
export interface CompiledModule {
  core: Uint8Array<ArrayBuffer>;
  /** A human-readable summary of how the program allocates memory. */
  allocationReport: string;
}

/** A TypeScript declaration file describing a world's imports and exports. */
export interface DeclarationFile {
  /** File name relative to the declarations directory, such as `world.d.ts`. */
  path: string;
  contents: string;
}

/** Broad, caller-actionable category of a compiler error. */
export type ErrorKind =
  | "invalid-input"
  | "io"
  | "wit"
  | "syntax"
  | "module"
  | "type-check"
  | "unsupported"
  | "contract"
  | "internal";

/**
 * A range in a source file.
 * Lines and columns start at 1 (columns count Unicode scalar values);
 * byte offsets are 0-based UTF-8 byte offsets.
 */
export interface SourceSpan {
  file: string;
  startLine: number;
  startColumn: number;
  endLine: number;
  endColumn: number;
  startByte: number;
  endByte: number;
}

/** An additional note attached to a {@link Diagnostic}, optionally anchored to a {@link SourceSpan}. */
export interface Note {
  message: string;
  span?: SourceSpan;
}

/** A problem the compiler found in its input. */
export interface Diagnostic {
  kind: ErrorKind;
  code: string;
  message: string;
  span?: SourceSpan;
  notes: Note[];
  help?: string;
}
