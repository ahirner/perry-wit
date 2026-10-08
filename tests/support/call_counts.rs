//! Instrument a separate component copy; counters never affect reported time or size.

use anyhow::{Context, Result};
use perry_wit::waffle_backend::WaffleCompiled;
use serde::Serialize;
use std::{collections::BTreeMap, future::Future, path::Path};
use waffle::{
    Export, ExportKind, FuncDecl, GlobalData, Operator as O,
    Type::{I32, I64},
};
use wasmtime::{
    Store,
    component::{Instance, TypedFunc},
};

#[allow(dead_code)]
#[path = "../../src/waffle_backend/runtime/builder.rs"]
mod builder;
#[path = "core_probe.rs"]
mod core_probe;

pub struct Counters {
    pub component: Vec<u8>,
    names: Vec<String>,
}

#[derive(Serialize)]
pub struct Counts {
    pub host_polls: u64,
    pub stream_reads: u64,
    pub worker_starts: u64,
    pub callbacks: u64,
    pub allocations: u64,
    pub reallocations: u64,
    pub root_frames: u64,
    pub promises: u64,
    pub collections: u64,
    pub heap_bumps: u64,
    pub boxed_values: u64,
    pub functions: BTreeMap<String, u64>,
}

impl Counters {
    pub fn new(
        compiled: &WaffleCompiled,
        world: &str,
        artifacts: &Path,
        name: &str,
    ) -> Result<Self> {
        let mut module = core_probe::named_core(compiled)?;
        let mut names = module
            .funcs
            .iter()
            .map(|id| (id, module.funcs[id].name().to_owned()))
            .collect::<BTreeMap<_, _>>();
        for import in &module.imports {
            if let waffle::ImportKind::Func(id) = import.kind {
                names.insert(id, format!("{}#{}", import.module, import.name));
            }
        }
        let globals = module
            .funcs
            .iter()
            .map(|id| {
                (
                    id,
                    module.globals.push(GlobalData {
                        ty: I64,
                        value: Some(0),
                        mutable: true,
                    }),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let imports = module
            .funcs
            .entries()
            .filter_map(|(id, function)| matches!(function, FuncDecl::Import(..)).then_some(id))
            .collect::<std::collections::BTreeSet<_>>();
        for (id, function) in module.funcs.entries_mut() {
            let FuncDecl::Body(_, _, body) = function else {
                continue;
            };
            let original = std::mem::take(&mut body.blocks[body.entry].insts);
            increment(body, body.entry, globals[&id]);
            body.blocks[body.entry].insts.extend(original);
            for block in body.blocks.iter().collect::<Vec<_>>() {
                let instructions = std::mem::take(&mut body.blocks[block].insts);
                for instruction in instructions {
                    if let waffle::ValueDef::Operator(O::Call { function_index }, _, _) =
                        body.values[instruction]
                        && imports.contains(&function_index)
                    {
                        increment(body, block, globals[&function_index]);
                    }
                    body.blocks[block].insts.push(instruction);
                }
            }
            body.validate()?;
        }
        let memory = module.memories.iter().next().context("guest memory")?;
        let getter = builder::declare(&mut module, "profile", &[I32], &[I64]);
        let mut b = builder::Builder::new(&module, getter, memory);
        let mut value = b.op(O::I64Const { value: 0 }, &[], I64);
        for (index, global) in globals.values().enumerate() {
            let index = b.integer(index as u32);
            let selected = b.op(O::I32Eq, &[b.param(0), index], I32);
            let count = b.op(
                O::GlobalGet {
                    global_index: *global,
                },
                &[],
                I64,
            );
            value = b.op(O::Select, &[count, value, selected], I64);
        }
        b.ret(&[value]);
        b.finish(&mut module, getter)?;
        module.exports.push(Export {
            name: "profile".into(),
            kind: ExportKind::Func(getter),
        });
        let wit = tempfile::tempdir()?;
        std::fs::write(
            wit.path().join("world.wit"),
            world.replace(
                "export run:",
                "export profile:func(index:u32)->u64; export run:",
            ),
        )?;
        let (resolve, package) = perry_wit::component::wit::resolve_wit(wit.path())?;
        let world = resolve.select_world(&[package], Some("task"))?;
        let component =
            perry_wit::waffle_backend::encode_component(&module.to_wasm_bytes()?, resolve, world)?;
        std::fs::write(
            artifacts.join(format!("{name}.instrumented.wasm")),
            &component,
        )?;
        Ok(Self {
            component,
            names: names.into_values().collect(),
        })
    }

    pub async fn snapshot<T: Send>(
        &self,
        store: &mut Store<T>,
        instance: &Instance,
    ) -> Result<Vec<u64>> {
        let getter: TypedFunc<(u32,), (u64,)> = instance.get_typed_func(&mut *store, "profile")?;
        let mut counts = Vec::new();
        for index in 0..self.names.len() {
            counts.push(getter.call_async(&mut *store, (index as u32,)).await?.0);
        }
        Ok(counts)
    }

    pub fn difference(&self, before: &[u64], after: &[u64], host_polls: u64) -> Counts {
        let count = |predicate: &dyn Fn(&str) -> bool| {
            self.names
                .iter()
                .zip(before)
                .zip(after)
                .filter(|((name, _), _)| predicate(name))
                .map(|((_, before), after)| after - before)
                .sum()
        };
        Counts {
            host_polls,
            stream_reads: count(&|name| name == "streams#read" || name.contains("[stream-read-")),
            worker_starts: count(&|name| name.contains("[thread-new")),
            callbacks: count(&|name| name.starts_with("[callback]")),
            allocations: count(&|name| name == "allocate"),
            reallocations: count(&|name| name == "realloc"),
            root_frames: count(&|name| name == "frame-new"),
            promises: count(&|name| name == "tasks.new"),
            collections: count(&|name| name == "collect"),
            heap_bumps: count(&|name| name == "heap.bump"),
            boxed_values: count(&|name| name == "value.new"),
            functions: self
                .names
                .iter()
                .zip(before)
                .zip(after)
                .enumerate()
                .filter(|(_, ((_, before), after))| after > before)
                .map(|(index, ((name, before), after))| {
                    (format!("func{index} {name}"), after - before)
                })
                .collect(),
        }
    }
}

pub async fn count_polls<F: Future>(future: F) -> (F::Output, u64) {
    let mut future = std::pin::pin!(future);
    let mut polls = 0;
    let result = std::future::poll_fn(|context| {
        polls += 1;
        future.as_mut().poll(context)
    })
    .await;
    (result, polls)
}

fn increment(body: &mut waffle::FunctionBody, block: waffle::Block, global: waffle::Global) {
    let current = body.add_op(
        block,
        O::GlobalGet {
            global_index: global,
        },
        &[],
        &[I64],
    );
    let one = body.add_op(block, O::I64Const { value: 1 }, &[], &[I64]);
    let next = body.add_op(block, O::I64Add, &[current, one], &[I64]);
    body.add_op(
        block,
        O::GlobalSet {
            global_index: global,
        },
        &[next],
        &[],
    );
}
