import { instantiate } from "../bindings/compiler.js";
import type * as Api from "../bindings/interfaces/perry-compiler-api.js";
import { loadCoreModule } from "#load-core";
import { CompileError } from "./errors.js";
import { wasiImports } from "./host.js";
import type {
  CompiledComponent,
  CompiledModule,
  CompileOptions,
  DeclarationFile,
  Diagnostic,
  ErrorKind,
  FileContents,
  Note,
  ProgramOptions,
  SourceFile,
  SourceSpan,
  WitOptions,
  WitPackage,
} from "./types.js";
import { entries, hasWasmMagic, owned, toBytes, type Same } from "./util.js";

/** The functions of the compiler's `perry:compiler/api` export. */
type Exports = Awaited<ReturnType<typeof instantiate>>["api"];

/**
 * A loaded compiler.
 *
 * Loading takes a moment, so {@link Compiler.load} is asynchronous;
 * every method of the loaded compiler then runs synchronously.
 * The module-level {@link compile}, {@link compileCore} and {@link generateTypes}
 * functions share one compiler that loads on first use.
 */
export class Compiler {
  /** The instantiated compiler's API. */
  readonly #api: Exports;

  /** Use {@link Compiler.load}, which instantiates the compiler first. */
  private constructor(api: Exports) {
    this.#api = api;
  }

  /** Load a compiler instance, independent of every other instance. */
  static async load(): Promise<Compiler> {
    const { api } = await instantiate(loadCoreModule, wasiImports());
    return new Compiler(api);
  }

  /**
   * Compile a TypeScript program into a WASI 0.3 component implementing a WIT world.
   * With `coreOnly: true`, return only the core module.
   * @throws {CompileError} when the program or its WIT is invalid.
   */
  compile(options: CompileOptions & { coreOnly: true }): CompiledModule;
  compile(options: CompileOptions & { coreOnly?: false }): CompiledComponent;
  compile(options: CompileOptions): CompiledComponent | CompiledModule;
  compile(options: CompileOptions): CompiledComponent | CompiledModule {
    const coreOnly = options.coreOnly ?? false;
    const compiled = call(() =>
      this.#api.compile(program(options), witInput(options), { world: options.world, coreOnly }),
    );
    if (coreOnly || !compiled.component) {
      return { core: owned(compiled.core), allocationReport: compiled.allocationReport };
    }
    return {
      component: owned(options.keepNames ? compiled.component.bytes : compiled.component.stripped),
      core: owned(compiled.core),
      allocationReport: compiled.allocationReport,
    };
  }

  /**
   * Compile a program's exports into a core WebAssembly module without a WIT world.
   * Exports keep their TypeScript names and take and return core numbers,
   * so this suits programs whose exports take and return only numbers.
   * @throws {CompileError} when the program is invalid.
   */
  compileCore(options: ProgramOptions): Uint8Array<ArrayBuffer> {
    return owned(call(() => this.#api.compileCore(program(options))));
  }

  /**
   * Generate TypeScript declarations for a WIT world's imports and exports,
   * so that a program can be type-checked against the world before compiling it.
   * @throws {CompileError} when the WIT is invalid.
   */
  generateTypes(options: WitOptions): DeclarationFile[] {
    const decoder = new TextDecoder();
    return call(() => this.#api.generateSdk(witInput(options), options.world)).map(({ name, contents }) => ({
      path: name,
      contents: decoder.decode(contents),
    }));
  }
}

/** The compiler behind the module-level functions, loaded on first use. */
let shared: Promise<Compiler> | undefined;

/**
 * Run `use` with the shared compiler, loading it first if needed.
 * A failed load discards the shared compiler, and so does any error other than a {@link CompileError},
 * so the next call loads a fresh one.
 * Only a {@link CompileError} shows that the compiler returned normally:
 * a trap, or a host exception such as the `RangeError` of an exhausted call stack,
 * unwinds through the compiler's frames and leaves its memory in an unknown state.
 */
async function withShared<T>(use: (compiler: Compiler) => T): Promise<T> {
  const loading = (shared ??= Compiler.load());
  let compiler: Compiler;
  try {
    compiler = await loading;
  } catch (error) {
    if (shared === loading) shared = undefined;
    throw error;
  }
  try {
    return use(compiler);
  } catch (error) {
    if (!(error instanceof CompileError) && shared === loading) shared = undefined;
    throw error;
  }
}

/**
 * Compile a TypeScript program into a WASI 0.3 component implementing a WIT world.
 * With `coreOnly: true`, return only the core module.
 *
 * ```ts
 * const { component } = await compile({
 *   source: "export function greet(name: string): string { return `Hello, ${name}!`; }",
 *   wit: "package example:greeter; world greeter { export greet: func(name: string) -> string; }",
 * });
 * ```
 *
 * @throws {CompileError} when the program or its WIT is invalid.
 */
export function compile(options: CompileOptions & { coreOnly: true }): Promise<CompiledModule>;
export function compile(options: CompileOptions & { coreOnly?: false }): Promise<CompiledComponent>;
export function compile(options: CompileOptions): Promise<CompiledComponent | CompiledModule>;
export function compile(options: CompileOptions): Promise<CompiledComponent | CompiledModule> {
  return withShared((compiler) => compiler.compile(options));
}

/**
 * Compile a program's exports into a core WebAssembly module without a WIT world.
 * @throws {CompileError} when the program is invalid.
 */
export function compileCore(options: ProgramOptions): Promise<Uint8Array<ArrayBuffer>> {
  return withShared((compiler) => compiler.compileCore(options));
}

/**
 * Generate TypeScript declarations for a WIT world's imports and exports.
 * @throws {CompileError} when the WIT is invalid.
 */
export function generateTypes(options: WitOptions): Promise<DeclarationFile[]> {
  return withShared((compiler) => compiler.generateTypes(options));
}

/** Convert a module given as contents or as a {@link SourceFile} into the compiler's `source` record. */
function sourceFile(input: FileContents | SourceFile): Api.Source {
  const file = typeof input === "string" || input instanceof Uint8Array ? { contents: input } : input;
  return { name: file.path, contents: toBytes(file.contents) };
}

/** Convert the entry module and the modules it imports into the compiler's `program` record. */
function program({ source, modules = {} }: ProgramOptions): Api.Program {
  return {
    source: sourceFile(source),
    modules: Array.from(entries(modules), ([name, contents]) => ({ name, contents: toBytes(contents) })),
  };
}

/** Convert a WIT package into the compiler's `wit-package` variant: encoded bytes, or source files. */
function witPackage(input: WitPackage): Api.WitPackage {
  if (hasWasmMagic(input)) return { tag: "encoded", val: input };
  const files = Array.isArray(input) ? input : [input as FileContents | SourceFile];
  return { tag: "sources", val: files.map(sourceFile) };
}

/** Convert the application's WIT package and its dependencies into the compiler's `wit-input` record. */
function witInput({ wit, witDependencies = [] }: WitOptions): Api.WitInput {
  return { application: witPackage(wit), dependencies: witDependencies.map(witPackage) };
}

/** Call into the compiler, rethrowing its diagnostics as a {@link CompileError}. */
function call<T>(run: () => T): T {
  try {
    return run();
  } catch (error) {
    const payload = (error as { payload?: unknown } | null)?.payload;
    if (Array.isArray(payload)) throw new CompileError(payload.map(diagnostic));
    throw error;
  }
}

/** Convert a WIT source span into a {@link SourceSpan}, with byte offsets as numbers rather than bigints. */
function sourceSpan(span: Api.SourceSpan): SourceSpan {
  return { ...span, startByte: Number(span.startByte), endByte: Number(span.endByte) };
}

/** Convert a compiler note into a {@link Note}. */
function note({ message, span }: Api.Note): Note {
  return span ? { message, span: sourceSpan(span) } : { message };
}

/** Convert a compiler diagnostic into a {@link Diagnostic}, with byte offsets as numbers rather than bigints. */
function diagnostic({ span, notes, ...rest }: Api.Diagnostic): Diagnostic {
  const mappedNotes = notes.map(note);
  if (!span) return { ...rest, notes: mappedNotes };
  return { ...rest, notes: mappedNotes, span: sourceSpan(span) };
}

// The public diagnostic types restate the compiler's WIT contract; fail the build when they drift.
true satisfies Same<Api.ErrorKind, ErrorKind>;
true satisfies Same<Omit<Api.Note, "span">, Omit<Note, "span">>;
true satisfies Same<keyof Api.Note, keyof Note>;
true satisfies Same<Omit<Api.Diagnostic, "span" | "notes">, Omit<Diagnostic, "span" | "notes">>;
true satisfies Same<keyof Api.Diagnostic, keyof Diagnostic>;
true satisfies Same<keyof Api.SourceSpan, keyof SourceSpan>;
