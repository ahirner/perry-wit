use crate::emit::{EmitOptions, Memory, emit_module};
use pliron::{
    builtin::op_interfaces::SingleBlockRegionInterface,
    builtin::ops::{FuncOp, ModuleOp},
    context::Context,
    irfmt::parsers::spaced,
    linked_list::ContainsLinkedList,
    operation::{Operation, verify_operation},
    parsable::parse_from_str,
    printable::Printable,
};
use wasmtime::{Engine, Instance, Store, Val};

fn compile_with(input: &str, options: &EmitOptions) -> Vec<u8> {
    let ctx = &mut Context::new();
    let module = parse_from_str(spaced(Operation::top_level_parser()), ctx, input)
        .unwrap_or_else(|error| panic!("{}", error.disp(ctx)));
    verify_operation(module, ctx).unwrap_or_else(|error| panic!("{}", error.disp(ctx)));
    let module = Operation::get_op::<ModuleOp>(module, ctx).expect("module");
    // Export every function under its symbol name.
    let body = module.get_body(ctx, 0);
    let functions: Vec<_> = body
        .deref(ctx)
        .iter(ctx)
        .filter_map(|op| Operation::get_op::<FuncOp>(op, ctx))
        .collect();
    for function in functions {
        use pliron::builtin::op_interfaces::SymbolOpInterface;
        let name = function.get_symbol_name(ctx).to_string();
        crate::ops::set_export(ctx, &function, &name);
    }
    let wasm = emit_module(ctx, &module, options).unwrap_or_else(|error| panic!("{error}"));
    wasmparser::Validator::new()
        .validate_all(&wasm)
        .unwrap_or_else(|error| panic!("{error}\n{}", wat(&wasm)));
    wasm
}

fn compile(input: &str) -> Vec<u8> {
    compile_with(
        input,
        &EmitOptions {
            memory: Memory::Define {
                min_pages: 1,
                export: None,
            },
            names: true,
        },
    )
}

fn wat(wasm: &[u8]) -> String {
    wasmprinter::print_bytes(wasm).unwrap_or_else(|error| format!("<{error}>"))
}

fn call(wasm: &[u8], function: &str, args: &[Val]) -> Vec<Val> {
    let engine = Engine::default();
    let module = wasmtime::Module::new(&engine, wasm).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let function = instance.get_func(&mut store, function).unwrap();
    let mut results = vec![Val::I32(0); function.ty(&store).results().len()];
    function
        .call(&mut store, args, &mut results)
        .unwrap_or_else(|error| panic!("{error:?}\n{}", wat(wasm)));
    results
}

fn i32_of(values: Vec<Val>) -> i32 {
    values[0].unwrap_i32()
}

const I32: &str = "builtin.integer i32";

/// `if`-`else` diamond with a join argument, and a loop with two loop-carried values.
fn loops() -> String {
    format!(
        r#"
builtin.module @m {{
  ^module():
    builtin.func @abs: builtin.function <({I32}) -> ({I32})> {{
      ^entry(x: {I32}):
        zero = builtin.constant <builtin.integer <0: i32>> : {I32};
        negative = wasm.num I32LtS x, zero : {I32};
        wasm.cond_br if negative ^neg() else ^join(x)
      ^neg():
        minus = wasm.num I32Sub zero, x : {I32};
        wasm.br ^join(minus)
      ^join(r: {I32}):
        wasm.return r
    }};
    builtin.func @sum_to: builtin.function <({I32}) -> ({I32})> {{
      ^entry(n: {I32}):
        zero = builtin.constant <builtin.integer <0: i32>> : {I32};
        wasm.br ^header(zero, zero)
      ^header(i: {I32}, acc: {I32}):
        more = wasm.num I32LtS i, n : {I32};
        wasm.cond_br if more ^body() else ^exit()
      ^body():
        one = builtin.constant <builtin.integer <1: i32>> : {I32};
        next_acc = wasm.num I32Add acc, i : {I32};
        next_i = wasm.num I32Add i, one : {I32};
        wasm.br ^header(next_i, next_acc)
      ^exit():
        wasm.return acc
    }};
    builtin.func @fib: builtin.function <({I32}) -> ({I32})> {{
      ^entry(n: {I32}):
        zero = builtin.constant <builtin.integer <0: i32>> : {I32};
        one = builtin.constant <builtin.integer <1: i32>> : {I32};
        wasm.br ^header(n, zero, one)
      ^header(k: {I32}, a: {I32}, b: {I32}):
        done = wasm.num I32Eqz k : {I32};
        wasm.cond_br if done ^exit() else ^body()
      ^body():
        c = wasm.num I32Add a, b : {I32};
        one2 = builtin.constant <builtin.integer <1: i32>> : {I32};
        k2 = wasm.num I32Sub k, one2 : {I32};
        wasm.br ^header(k2, b, c)
      ^exit():
        wasm.return a
    }};
    builtin.func @swap_parity: builtin.function <({I32}) -> ({I32})> {{
      ^entry(n: {I32}):
        zero = builtin.constant <builtin.integer <0: i32>> : {I32};
        one = builtin.constant <builtin.integer <1: i32>> : {I32};
        wasm.br ^header(n, zero, one)
      ^header(k: {I32}, x: {I32}, y: {I32}):
        done = wasm.num I32Eqz k : {I32};
        wasm.cond_br if done ^exit() else ^body()
      ^body():
        one2 = builtin.constant <builtin.integer <1: i32>> : {I32};
        k2 = wasm.num I32Sub k, one2 : {I32};
        wasm.br ^header(k2, y, x)
      ^exit():
        wasm.return x
    }}
}}
"#
    )
}

#[test]
fn diamonds_loops_and_cyclic_block_arguments_run() {
    let wasm = compile(&loops());
    assert_eq!(i32_of(call(&wasm, "abs", &[Val::I32(-7)])), 7);
    assert_eq!(i32_of(call(&wasm, "abs", &[Val::I32(5)])), 5);
    assert_eq!(i32_of(call(&wasm, "sum_to", &[Val::I32(100)])), 4950);
    assert_eq!(i32_of(call(&wasm, "fib", &[Val::I32(20)])), 6765);
    // `^header(k2, y, x)` swaps two parameters on the back edge.
    assert_eq!(i32_of(call(&wasm, "swap_parity", &[Val::I32(7)])), 1);
    assert_eq!(i32_of(call(&wasm, "swap_parity", &[Val::I32(8)])), 0);
}

/// Nested loops, an early exit from the inner loop (`break`), and a `continue` edge.
#[test]
fn nested_loops_with_break_and_continue_run() {
    let input = format!(
        r#"
builtin.module @m {{
  ^module():
    builtin.func @pairs: builtin.function <({I32}) -> ({I32})> {{
      ^entry(n: {I32}):
        zero = builtin.constant <builtin.integer <0: i32>> : {I32};
        wasm.br ^outer(zero, zero)
      ^outer(i: {I32}, count: {I32}):
        more = wasm.num I32LtS i, n : {I32};
        wasm.cond_br if more ^inner_entry() else ^exit()
      ^inner_entry():
        wasm.br ^inner(zero, count)
      ^inner(j: {I32}, c: {I32}):
        inner_more = wasm.num I32LtS j, i : {I32};
        wasm.cond_br if inner_more ^inner_body() else ^outer_latch(c)
      ^inner_body():
        one = builtin.constant <builtin.integer <1: i32>> : {I32};
        j1 = wasm.num I32Add j, one : {I32};
        seven = builtin.constant <builtin.integer <7: i32>> : {I32};
        is_seven = wasm.num I32Eq j, seven : {I32};
        wasm.cond_br if is_seven ^outer_latch(c) else ^count_it()
      ^count_it():
        three = builtin.constant <builtin.integer <3: i32>> : {I32};
        rem = wasm.num I32RemU j, three : {I32};
        skip = wasm.num I32Eqz rem : {I32};
        wasm.cond_br if skip ^inner(j1, c) else ^bump()
      ^bump():
        one3 = builtin.constant <builtin.integer <1: i32>> : {I32};
        c1 = wasm.num I32Add c, one3 : {I32};
        wasm.br ^inner(j1, c1)
      ^outer_latch(total: {I32}):
        one2 = builtin.constant <builtin.integer <1: i32>> : {I32};
        i1 = wasm.num I32Add i, one2 : {I32};
        wasm.br ^outer(i1, total)
      ^exit():
        wasm.return count
    }}
}}
"#
    );
    let wasm = compile(&input);
    let reference = |n: i32| {
        let mut count = 0;
        for i in 0..n {
            for j in 0..i {
                if j == 7 {
                    break;
                }
                if j % 3 == 0 {
                    continue;
                }
                count += 1;
            }
        }
        count
    };
    for n in [0, 1, 5, 12, 30] {
        assert_eq!(
            i32_of(call(&wasm, "pairs", &[Val::I32(n)])),
            reference(n),
            "n={n}"
        );
    }
}

#[test]
fn calls_recursion_memory_and_floats_run() {
    let f64t = "builtin.fp64";
    let input = format!(
        r#"
builtin.module @m {{
  ^module():
    builtin.func @fact: builtin.function <({I32}) -> ({I32})> {{
      ^entry(n: {I32}):
        one = builtin.constant <builtin.integer <1: i32>> : {I32};
        small = wasm.num I32LeS n, one : {I32};
        wasm.cond_br if small ^base() else ^step()
      ^base():
        wasm.return one
      ^step():
        m = wasm.num I32Sub n, one : {I32};
        r = wasm.call fact (m) : {I32};
        p = wasm.num I32Mul n, r : {I32};
        wasm.return p
    }};
    builtin.func @mem: builtin.function <({f64t}) -> ({f64t})> {{
      ^entry(x: {f64t}):
        base = builtin.constant <builtin.integer <64: i32>> : {I32};
        wasm.store F64 base + <8: i32> = x;
        y = wasm.load F64 base + <8: i32> : {f64t};
        half = builtin.constant <builtin.double 0.5> : {f64t};
        z = wasm.num F64Mul y, half : {f64t};
        neg = wasm.num F64Neg z : {f64t};
        big = wasm.num F64Gt x, half : {I32};
        r = wasm.select big ? z : neg : {f64t};
        wasm.return r
    }}
}}
"#
    );
    let wasm = compile(&input);
    assert_eq!(i32_of(call(&wasm, "fact", &[Val::I32(10)])), 3628800);
    assert_eq!(
        call(&wasm, "mem", &[Val::F64(3f64.to_bits())])[0].unwrap_f64(),
        1.5
    );
    assert_eq!(
        call(&wasm, "mem", &[Val::F64(0.25f64.to_bits())])[0].unwrap_f64(),
        -0.125
    );
}

#[test]
fn imports_and_imported_memory_link() {
    let input = format!(
        r#"
builtin.module @m {{
  ^module():
    wasm.import_func @twice: builtin.function <({I32}) -> ({I32})> from "host" "twice";
    builtin.func @quad: builtin.function <({I32}) -> ({I32})> {{
      ^entry(x: {I32}):
        a = wasm.call twice (x) : {I32};
        b = wasm.call twice (a) : {I32};
        wasm.store I32 x + <0: i32> = b;
        wasm.return b
    }}
}}
"#
    );
    let wasm = compile_with(
        &input,
        &EmitOptions {
            memory: Memory::Import {
                module: "host".into(),
                name: "memory".into(),
                min_pages: 1,
            },
            names: false,
        },
    );
    let engine = Engine::default();
    let module = wasmtime::Module::new(&engine, &wasm).unwrap();
    let mut store = Store::new(&engine, ());
    let memory = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(1, None)).unwrap();
    let twice = wasmtime::Func::wrap(&mut store, |x: i32| x * 2);
    let instance = Instance::new(&mut store, &module, &[twice.into(), memory.into()]).unwrap();
    let quad = instance
        .get_typed_func::<i32, i32>(&mut store, "quad")
        .unwrap();
    assert_eq!(quad.call(&mut store, 16).unwrap(), 64);
    assert_eq!(memory.data(&store)[16..20], 64i32.to_le_bytes());
}

/// Single-use pure values are evaluated in place, so straight-line arithmetic needs no locals.
#[test]
fn single_use_values_stay_on_the_stack() {
    let input = format!(
        r#"
builtin.module @m {{
  ^module():
    builtin.func @poly: builtin.function <({I32}, {I32}, {I32}) -> ({I32})> {{
      ^entry(a: {I32}, b: {I32}, c: {I32}):
        bc = wasm.num I32Mul b, c : {I32};
        s = wasm.num I32Add a, bc : {I32};
        two = builtin.constant <builtin.integer <2: i32>> : {I32};
        r = wasm.num I32Shl s, two : {I32};
        wasm.return r
    }}
}}
"#
    );
    let wasm = compile(&input);
    let text = wat(&wasm);
    assert!(!text.contains("local.set"), "{text}");
    assert!(!text.contains("(local "), "{text}");
    assert_eq!(
        i32_of(call(
            &wasm,
            "poly",
            &[Val::I32(1), Val::I32(2), Val::I32(3)]
        )),
        28
    );
}

/// A trapping or memory-reading op never moves across a store: the load below must read the
/// old value even though its only use comes after the store.
#[test]
fn effects_keep_their_order() {
    let input = format!(
        r#"
builtin.module @m {{
  ^module():
    builtin.func @order: builtin.function <({I32}) -> ({I32})> {{
      ^entry(x: {I32}):
        addr = builtin.constant <builtin.integer <32: i32>> : {I32};
        wasm.store I32 addr + <0: i32> = x;
        old = wasm.load I32 addr + <0: i32> : {I32};
        one = builtin.constant <builtin.integer <1: i32>> : {I32};
        bumped = wasm.num I32Add x, one : {I32};
        wasm.store I32 addr + <0: i32> = bumped;
        quotient = wasm.num I32DivU old, x : {I32};
        wasm.return quotient
    }}
}}
"#
    );
    let wasm = compile(&input);
    assert_eq!(i32_of(call(&wasm, "order", &[Val::I32(5)])), 1);
}

#[test]
fn irreducible_control_flow_is_rejected() {
    let input = format!(
        r#"
builtin.module @m {{
  ^module():
    builtin.func @irreducible: builtin.function <({I32}) -> ({I32})> {{
      ^entry(x: {I32}):
        wasm.cond_br if x ^a() else ^b()
      ^a():
        wasm.br ^b()
      ^b():
        wasm.cond_br if x ^a() else ^exit()
      ^exit():
        wasm.return x
    }}
}}
"#
    );
    let ctx = &mut Context::new();
    let module = parse_from_str(spaced(Operation::top_level_parser()), ctx, &input).unwrap();
    verify_operation(module, ctx).unwrap();
    let module = Operation::get_op::<ModuleOp>(module, ctx).unwrap();
    let error = emit_module(ctx, &module, &EmitOptions::default()).unwrap_err();
    assert!(error.to_string().contains("irreducible"), "{error}");
}

#[test]
fn dialect_round_trips_through_text() {
    let ctx = &mut Context::new();
    let module = parse_from_str(spaced(Operation::top_level_parser()), ctx, &loops()).unwrap();
    let printed = Operation::get_op_dyn(module, ctx).disp(ctx).to_string();
    for expected in [
        "wasm.num I32LtS",
        "wasm.cond_br if",
        "wasm.br ^",
        "wasm.return",
    ] {
        assert!(printed.contains(expected), "{printed}");
    }
    let ctx2 = &mut Context::new();
    let reparsed = parse_from_str(
        spaced(Operation::top_level_parser()),
        ctx2,
        &regex_lite(&printed),
    )
    .unwrap_or_else(|error| panic!("{}", error.disp(ctx2)));
    verify_operation(reparsed, ctx2).unwrap();
}

/// Drop printer-only location references (` !N`).
fn regex_lite(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' && chars.peek() == Some(&'!') {
            let mut lookahead = chars.clone();
            lookahead.next();
            if lookahead.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    chars.next();
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

#[test]
#[ignore]
fn dump_loops() {
    println!("{}", wat(&compile(&loops())));
}
