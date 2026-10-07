//! Callback exports keep native operation owners alive through cancellation acknowledgement.

use super::builder::{self, Builder};
use super::pending_result::PendingExportResult;
use super::scheduler::{self, HostTurn};
use crate::waffle_backend::{
    registry::{FunctionExport, ModuleRegistry},
    resolve::ResolvedContract,
    strings::StringPool,
    wit::{WitExport, WitWorld},
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{
    Export, ExportKind, Func, Import, ImportKind, Module, Operator as O, SignatureData, TableData,
    Type::{self, I32},
};

const CONTEXT: u32 = 124;
const DONE: u32 = 128;
const FRAME: u32 = 132;

pub(crate) fn enabled(contract: &ResolvedContract) -> bool {
    contract.wit.as_ref().is_some_and(|wit| {
        wit.functions
            .values()
            .any(|export| export.function.kind.is_async())
    }) && contract.http_handler.is_none()
}

pub(crate) struct Imports {
    returns: BTreeMap<String, Func>,
    cancel: Func,
    new_thread: Func,
    resume: Func,
}
pub(crate) fn declare(module: &mut Module<'static>, contract: &ResolvedContract) -> Imports {
    let wit = contract.wit.as_ref().unwrap();
    let mut returns = BTreeMap::new();
    for export in wit
        .functions
        .values()
        .filter(|export| export.function.kind.is_async())
    {
        let (mut namespace, name, signature) =
            export
                .function
                .task_return_import(&wit.resolve, None, wit_parser::Mangling::Legacy);
        if let Some((interface, _)) = export.core_name.split_once('#') {
            namespace = format!("[export]{interface}");
        }
        let signature = module.signatures.push(SignatureData {
            params: signature
                .params
                .into_iter()
                .map(crate::waffle_backend::wit::core_type)
                .collect(),
            returns: vec![],
        });
        let function = module
            .funcs
            .push(waffle::FuncDecl::Import(signature, name.clone()));
        module.imports.push(Import {
            module: namespace,
            name,
            kind: ImportKind::Func(function),
        });
        returns.insert(export.core_name.clone(), function);
    }
    let cancel = super::imports::declare_imports(
        module,
        "[export]$root",
        &[super::imports::Function {
            name: "[task-cancel]".into(),
            params: vec![],
            results: vec![],
        }],
    )["[task-cancel]"];
    Imports {
        returns,
        cancel,
        new_thread: builder::native(module, "[thread-new-indirect-v0]", &[I32; 2], &[I32]),
        resume: builder::native(module, "[thread-yield-then-resume]", &[I32], &[I32]),
    }
}

pub(crate) fn emit(
    module: &mut Module<'static>,
    registry: &ModuleRegistry,
    wit: &WitWorld,
    declaration: &WitExport,
    export: &FunctionExport,
    strings: &StringPool,
) -> Result<()> {
    let imports = registry.callbacks.as_ref().unwrap();
    let operations = registry.operations.unwrap();
    let allocator = registry.allocator.unwrap();
    let memory = registry.memory;
    let signature = module.signatures[export.sig].clone();
    let publisher = crate::waffle_backend::wit::build_task_return(
        module,
        registry,
        wit,
        declaration,
        export,
        imports.returns[&export.name],
        strings,
    )?;
    let worker = builder::declare(module, &format!("{}.worker", export.name), &[I32], &[]);
    let result_offset = 8 * signature.params.len() as u32;
    let mut b = Builder::new(module, worker, memory);
    let context = b.param(0);
    let args = signature
        .params
        .iter()
        .enumerate()
        .map(|(index, ty)| b.load(context, 8 * index as u32, *ty))
        .collect::<Vec<_>>();
    let returned = b.call(export.func_index, &args, &signature.returns);
    for (index, (value, ty)) in returned.iter().zip(&signature.returns).enumerate() {
        b.store(context, result_offset + 8 * index as u32, *value, *ty);
    }
    if let Some(cancel) = registry.fetch_helpers.and_then(|fetch| fetch.cancel_unused) {
        b.call(cancel, &[], &[]);
    }
    if let Some(promises) = &registry.promises {
        b.call(promises.native.scheduler.complete_source, &[], &[]);
    }
    let address = b.integer(DONE);
    let one = b.integer(1);
    b.store(address, 0, one, I32);
    b.ret(&[]);
    b.finish(module, worker)?;
    let table = if let Some(table) = module.tables.iter().next() {
        table
    } else {
        let table = module.tables.push(TableData {
            ty: Type::FuncRef,
            initial: 0,
            max: Some(0),
            func_elements: Some(vec![]),
        });
        module.exports.push(Export {
            name: "__indirect_function_table".into(),
            kind: ExportKind::Table(table),
        });
        table
    };
    let table = &mut module.tables[table];
    let elements = table.func_elements.as_mut().unwrap();
    let index = elements.len() as u32;
    elements.push(worker);
    table.initial = elements.len() as u64;
    table.max = Some(table.initial);

    let publish = builder::declare(module, &format!("{}.publish", export.name), &[], &[I32]);
    let mut b = Builder::new(module, publish, memory);
    let address = b.integer(DONE);
    let done = b.load(address, 0, I32);
    let finished = scheduler::can_publish(&mut b, operations, done);
    let finish = b.body.add_block();
    let wait = b.body.add_block();
    b.branch(finished, finish, wait);
    b.block = wait;
    let pending = b.integer(1);
    b.ret(&[pending]);
    b.block = finish;
    if let Some(fetch) = registry.fetch_helpers {
        b.call(fetch.finish, &[], &[]);
    }
    if let Some(promises) = &registry.promises {
        b.call(promises.native.finish, &[], &[]);
    }
    let cancelled = b.call(operations.finish, &[], &[I32])[0];
    let cancel = b.body.add_block();
    let success = b.body.add_block();
    let cleanup = b.body.add_block();
    b.branch(cancelled, cancel, success);
    b.block = cancel;
    b.call(imports.cancel, &[], &[]);
    b.jump(cleanup, &[]);
    b.block = success;
    let address = b.integer(CONTEXT);
    let context = b.load(address, 0, I32);
    let returned = signature
        .returns
        .iter()
        .enumerate()
        .map(|(index, ty)| b.load(context, result_offset + 8 * index as u32, *ty))
        .collect::<Vec<_>>();
    b.call(publisher, &returned, &[]);
    b.jump(cleanup, &[]);
    b.block = cleanup;
    PendingExportResult::release(&mut b, allocator, memory);
    // No guest thread or native operation may retain an invocation frame here.
    let next = b.body.add_block();
    let release = b.body.add_block();
    let done = b.body.add_block();
    b.jump(next, &[]);
    b.block = next;
    let frames = b.integer(44);
    let frame = b.load(frames, 0, I32);
    b.branch(frame, release, done);
    b.block = release;
    b.call(allocator.frame_drop, &[frame], &[]);
    b.jump(next, &[]);
    b.block = done;
    let zero = b.integer(0);
    for address in [CONTEXT, DONE, FRAME, 32] {
        let address = b.integer(address);
        b.store(address, 0, zero, I32);
    }
    b.call(allocator.post_return, &[], &[]);
    b.ret(&[zero]);
    b.finish(module, publish)?;

    let entry_name = format!("[async-lift]{}", export.name);
    let entry = builder::declare(module, &entry_name, &signature.params, &[I32]);
    let mut b = Builder::new(module, entry, memory);
    b.call(operations.enter, &[], &[]);
    PendingExportResult::require_vacant(&mut b);
    let references = signature.params.iter().filter(|ty| **ty == I32).count() as u32;
    let count = b.integer(references + 1);
    let frame = b.call(allocator.frame_new, &[count], &[I32])[0];
    let address = b.integer(FRAME);
    b.store(address, 0, frame, I32);
    let context = b.allocate(
        allocator.realloc,
        (result_offset + 8 * signature.returns.len() as u32).max(8),
        8,
    );
    b.store(frame, 12, context, I32);
    let mut root = 16;
    for (index, ty) in signature.params.iter().enumerate() {
        b.store(context, 8 * index as u32, b.param(index), *ty);
        if *ty == I32 {
            b.store(frame, root, b.param(index), I32);
            root += 4;
        }
    }
    let address = b.integer(CONTEXT);
    b.store(address, 0, context, I32);
    let zero = b.integer(0);
    let done = b.integer(DONE);
    b.store(done, 0, zero, I32);
    scheduler::reset_workers(&mut b);
    let index = b.integer(index);
    let thread = b.call(imports.new_thread, &[index, context], &[I32])[0];
    b.call(imports.resume, &[thread], &[I32]);
    let action = scheduler::host_action(&mut b, operations, HostTurn::Dispatch);
    b.ret(&[action]);
    b.finish(module, entry)?;

    let callback_name = format!("[callback]{entry_name}");
    let callback = builder::declare(module, &callback_name, &[I32; 3], &[I32]);
    let mut b = Builder::new(module, callback, memory);
    let six = b.integer(6);
    let cancelled = b.op(O::I32Eq, &[b.param(0), six], I32);
    let cancel = b.body.add_block();
    let inspect = b.body.add_block();
    let finish = b.body.add_block();
    b.branch(cancelled, cancel, inspect);
    b.block = cancel;
    b.call(operations.cancel_all, &[], &[]);
    if let Some(promises) = &registry.promises {
        b.call(promises.native.cancel_all, &[], &[]);
    }
    b.jump(finish, &[]);
    b.block = inspect;
    let notify = b.body.add_block();
    b.branch(b.param(0), notify, finish);
    b.block = notify;
    b.call(
        operations.notify,
        &[b.param(0), b.param(1), b.param(2)],
        &[],
    );
    b.jump(finish, &[]);
    b.block = finish;
    let pending = b.call(publish, &[], &[I32])[0];
    let running = b.body.add_block();
    let finished = b.body.add_block();
    b.branch(pending, running, finished);
    b.block = finished;
    let zero = b.integer(0);
    b.ret(&[zero]);
    b.block = running;
    let turn = if registry.promises.is_some() {
        let done = b.integer(DONE);
        HostTurn::SourceCallback {
            event: b.param(0),
            source_done: b.load(done, 0, I32),
        }
    } else {
        HostTurn::Dispatch
    };
    let action = scheduler::host_action(&mut b, operations, turn);
    b.ret(&[action]);
    b.finish(module, callback)?;
    for (name, function) in [(entry_name, entry), (callback_name, callback)] {
        module.exports.push(Export {
            name,
            kind: ExportKind::Func(function),
        });
    }
    Ok(())
}
