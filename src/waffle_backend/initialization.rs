//! Once-per-instance module evaluation with retained, typed lexical bindings.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use perry_hir::{
    ir::{Function, Module as HirModule, Stmt},
    types::{FuncId, LocalId, Type as HirType},
};
use perry_parser::{parse_typescript, swc_ecma_ast as ast};
use waffle::{Func, Global, GlobalData, Module, Operator, Type};

use super::{
    registry::{ModuleRegistry, map_type_to_waffle},
    runtime::builder::{self, Builder},
    wit::WitWorld,
};

const INITIALIZER: &str = "__perry_instance_initialize";

#[repr(u32)]
enum EvaluationState {
    Uninitialized,
    Running,
    Ready,
    Failed,
}

pub(super) fn prepare_command(module: &mut ast::Module, wit: &mut WitWorld) -> Result<()> {
    let command = wit
        .functions
        .iter()
        .find(|(_, export)| export.core_name == "wasi:cli/run@0.3.0#run")
        .map(|(name, _)| name.clone());
    let Some(name) = command else {
        return Ok(());
    };
    if module.body.iter().any(|item| matches!(item, ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(ast::ExportDecl { decl: ast::Decl::Fn(function), .. })) if function.ident.sym == name)) {
        return Ok(());
    }
    let mut adapter = parse_typescript(
        "export function __perry_command(): {ok:true}|{ok:false} { return {ok:true}; }",
        "command-adapter.ts",
    )
    .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let export = wit.functions.remove(&name).unwrap();
    wit.functions.insert("__perry_command".into(), export);
    module.body.append(&mut adapter.body);
    Ok(())
}

pub(super) fn extract(hir: &mut HirModule) -> Result<()> {
    if hir.init.is_empty() {
        return Ok(());
    }
    ensure!(
        !hir.functions
            .iter()
            .any(|function| function.name == INITIALIZER),
        "Reserved module initializer name"
    );
    let id = hir
        .functions
        .iter()
        .map(|function| function.id)
        .max()
        .map_or(0, |id| id + 1);
    hir.functions.push(Function {
        id,
        name: INITIALIZER.into(),
        type_params: vec![],
        params: vec![],
        return_type: HirType::Void,
        body: std::mem::take(&mut hir.init),
        is_async: false,
        is_generator: false,
        is_strict: true,
        is_exported: false,
        captures: vec![],
        decorators: vec![],
        was_plain_async: false,
        was_unrolled: false,
    });
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct ModulePlan {
    pub(crate) function: FuncId,
    pub(crate) bindings: BTreeMap<LocalId, HirType>,
}

impl ModulePlan {
    pub(super) fn from_hir(hir: &HirModule) -> Option<Self> {
        let initializer = hir
            .functions
            .iter()
            .find(|function| function.name == INITIALIZER)?;
        let mut bindings = BTreeMap::new();
        super::visit::visit_statement_nodes(&initializer.body, &mut |stmt| {
            if let Stmt::Let { id, ty, .. } = stmt {
                bindings.insert(*id, ty.clone());
            }
        });
        let mut captured = BTreeSet::new();
        for function in &hir.functions {
            if function.id != initializer.id {
                super::visit::visit_function_expressions(function, &mut |expression| {
                    if let perry_hir::ir::Expr::LocalGet(id)
                    | perry_hir::ir::Expr::LocalSet(id, _)
                    | perry_hir::ir::Expr::Update { id, .. } = expression
                    {
                        captured.insert(*id);
                    }
                });
            }
        }
        bindings.retain(|id, _| captured.contains(id));
        Some(Self {
            function: initializer.id,
            bindings,
        })
    }
}

#[derive(Clone)]
pub(crate) struct Binding {
    pub(crate) value: Global,
    pub(crate) initialized: Global,
    pub(crate) ty: HirType,
    pub(crate) root: Option<u32>,
}

pub(crate) struct ModuleState {
    pub(crate) bindings: BTreeMap<LocalId, Binding>,
    pub(crate) roots: Global,
    pub(crate) evaluate: Func,
    state: Global,
    failure: Global,
}

impl ModuleState {
    pub(crate) fn declare(module: &mut Module<'static>, plan: &ModulePlan) -> Result<Self> {
        let mut root_count = 0;
        let mut bindings = BTreeMap::new();
        for (id, ty) in &plan.bindings {
            ensure!(
                *ty != HirType::Any && *ty != HirType::Unknown,
                "Module binding {id} requires a statically known type"
            );
            let root = if super::ssa::types::is_reference(ty) {
                let slot = root_count;
                root_count += 1;
                Some(slot)
            } else {
                None
            };
            bindings.insert(
                *id,
                Binding {
                    value: global(module, map_type_to_waffle(ty)?),
                    initialized: global(module, Type::I32),
                    ty: ty.clone(),
                    root,
                },
            );
        }
        Ok(Self {
            bindings,
            roots: global(module, Type::I32),
            state: global(module, Type::I32),
            failure: global(module, Type::F64),
            evaluate: builder::declare(module, "module.evaluate", &[], &[Type::I32, Type::F64]),
        })
    }

    pub(crate) fn emit(
        &self,
        module: &mut Module<'static>,
        plan: &ModulePlan,
        registry: &ModuleRegistry,
    ) -> Result<()> {
        let mut b = Builder::new(module, self.evaluate, registry.memory);
        let state = b.op(
            Operator::GlobalGet {
                global_index: self.state,
            },
            &[],
            Type::I32,
        );
        let start = b.body.add_block();
        let done = b.body.add_block();
        let zero = b.integer(EvaluationState::Uninitialized as u32);
        let fresh = b.op(Operator::I32Eq, &[state, zero], Type::I32);
        b.branch(fresh, start, done);
        b.block = done;
        let ready = b.integer(EvaluationState::Ready as u32);
        let succeeded = b.op(Operator::I32Eq, &[state, ready], Type::I32);
        let failed = b.integer(EvaluationState::Failed as u32);
        let failed = b.op(Operator::I32Eq, &[state, failed], Type::I32);
        let terminal = b.op(Operator::I32Or, &[succeeded, failed], Type::I32);
        b.require(terminal);
        let error = b.op(
            Operator::GlobalGet {
                global_index: self.failure,
            },
            &[],
            Type::F64,
        );
        b.ret(&[failed, error]);
        b.block = start;
        let running = b.integer(EvaluationState::Running as u32);
        b.effect(
            Operator::GlobalSet {
                global_index: self.state,
            },
            &[running],
        );
        let count = self
            .bindings
            .values()
            .filter(|binding| binding.root.is_some())
            .count();
        if count != 0 {
            let count = b.integer(count as u32);
            let roots = b.call(
                registry.allocator.unwrap().retained_frame_new,
                &[count],
                &[Type::I32],
            )[0];
            b.effect(
                Operator::GlobalSet {
                    global_index: self.roots,
                },
                &[roots],
            );
        }
        let completion = b.call(
            registry.functions[&plan.function].func_index,
            &[],
            &[Type::I32, Type::F64],
        );
        let ready = b.integer(EvaluationState::Ready as u32);
        let state = b.op(Operator::I32Add, &[ready, completion[0]], Type::I32);
        b.effect(
            Operator::GlobalSet {
                global_index: self.state,
            },
            &[state],
        );
        b.effect(
            Operator::GlobalSet {
                global_index: self.failure,
            },
            &[completion[1]],
        );
        b.ret(&completion);
        b.finish(module, self.evaluate)
    }
}

fn global(module: &mut Module<'static>, ty: Type) -> Global {
    module.globals.push(GlobalData {
        ty,
        value: Some(0),
        mutable: true,
    })
}
