//! Test-only allocation counters; production size and timing use uninstrumented code.
use anyhow::{Context, Result, ensure};
use waffle::{
    Export, ExportKind, FuncDecl, GlobalData, Module, Operator as O, Terminator,
    Type::{I32, I64},
    ValueDef,
};
#[allow(dead_code)]
#[path = "../../src/waffle_backend/runtime/builder.rs"]
mod builder;

pub fn instrument(core: &[u8]) -> Result<Vec<u8>> {
    let mut module = Module::from_wasm_bytes(core, &Default::default())?;
    module.expand_all_funcs()?;
    let mut module = module.without_orig_bytes();
    let realloc = module
        .exports
        .iter()
        .find_map(|e| match e.kind {
            ExportKind::Func(f) if e.name == "cabi_realloc" => Some(f),
            _ => None,
        })
        .context("missing allocator")?;
    let FuncDecl::Body(_, _, body) = &module.funcs[realloc] else {
        anyhow::bail!("allocator is not defined")
    };
    let allocate = body
        .values
        .entries()
        .filter_map(|(_, v)| match v {
            ValueDef::Operator(O::Call { function_index }, _, _)
                if module.signatures[module.funcs[*function_index].sig()].params == [I32, I32] =>
            {
                Some(*function_index)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    ensure!(
        allocate.len() == 1,
        "expected one shared allocation primitive"
    );
    let allocate = allocate[0];
    let memory = module.memories.iter().next().context("missing heap")?;
    let globals = [0; 3].map(|_| {
        module.globals.push(GlobalData {
            ty: I64,
            value: Some(0),
            mutable: true,
        })
    });
    for (name, global) in ["measure-allocations", "measure-bytes", "measure-peak"]
        .into_iter()
        .zip(globals)
    {
        let f = builder::declare(&mut module, name, &[], &[I64]);
        let mut b = builder::Builder::new(&module, f, memory);
        let value = b.op(
            O::GlobalGet {
                global_index: global,
            },
            &[],
            I64,
        );
        b.ret(&[value]);
        b.finish(&mut module, f)?;
        module.exports.push(Export {
            name: name.into(),
            kind: ExportKind::Func(f),
        });
    }
    let scan = builder::declare(&mut module, "measure.heap-usage", &[], &[I64]);
    let mut b = builder::Builder::new(&module, scan, memory);
    let address = b.integer(36);
    let head = b.load(address, 0, I32);
    let zero = b.op(O::I64Const { value: 0 }, &[], I64);
    let next = b.body.add_block();
    let block = b.body.add_blockparam(next, I32);
    let total = b.body.add_blockparam(next, I64);
    let visit = b.body.add_block();
    let done = b.body.add_block();
    b.jump(next, &[head, zero]);
    b.block = next;
    b.branch(block, visit, done);
    b.block = visit;
    let kind = b.load(block, 16, I32);
    let free = b.integer(u32::MAX);
    let used = b.op(O::I32Ne, &[kind, free], I32);
    let size = b.load(block, 12, I32);
    let size = b.op(O::I64ExtendI32U, &[size], I64);
    let size = b.op(O::Select, &[size, zero, used], I64);
    let updated_total = b.op(O::I64Add, &[total, size], I64);
    let following = b.load(block, 0, I32);
    b.jump(next, &[following, updated_total]);
    b.block = done;
    b.ret(&[total]);
    b.finish(&mut module, scan)?;
    let FuncDecl::Body(_, _, body) = &mut module.funcs[allocate] else {
        unreachable!()
    };
    let entry = body.entry;
    let original = std::mem::take(&mut body.blocks[entry].insts);
    for (global, value) in [
        (globals[0], None),
        (globals[1], Some(body.blocks[entry].params[1].1)),
    ] {
        let increment = if let Some(size) = value {
            body.add_op(entry, O::I64ExtendI32U, &[size], &[I64])
        } else {
            let size = body.blocks[entry].params[1].1;
            let zero = body.add_op(entry, O::I32Const { value: 0 }, &[], &[I32]);
            let nonempty = body.add_op(entry, O::I32Ne, &[size, zero], &[I32]);
            body.add_op(entry, O::I64ExtendI32U, &[nonempty], &[I64])
        };
        let current = body.add_op(
            entry,
            O::GlobalGet {
                global_index: global,
            },
            &[],
            &[I64],
        );
        let updated = body.add_op(entry, O::I64Add, &[current, increment], &[I64]);
        body.add_op(
            entry,
            O::GlobalSet {
                global_index: global,
            },
            &[updated],
            &[],
        );
    }
    body.blocks[entry].insts.extend(original);
    for block in body.blocks.iter().collect::<Vec<_>>() {
        if !matches!(body.blocks[block].terminator, Terminator::Return { .. }) {
            continue;
        }
        let used = body.add_op(
            block,
            O::Call {
                function_index: scan,
            },
            &[],
            &[I64],
        );
        let peak = body.add_op(
            block,
            O::GlobalGet {
                global_index: globals[2],
            },
            &[],
            &[I64],
        );
        let larger = body.add_op(block, O::I64GtU, &[used, peak], &[I32]);
        let peak = body.add_op(block, O::Select, &[used, peak, larger], &[I64]);
        body.add_op(
            block,
            O::GlobalSet {
                global_index: globals[2],
            },
            &[peak],
            &[],
        );
    }
    body.validate()?;
    module.to_wasm_bytes()
}
