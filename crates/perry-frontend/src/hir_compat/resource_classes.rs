//! HIR adapters for class instances crossing canonical resource boundaries.
use super::{
    Witness, mark_non_null,
    witnesses::{ClassCall, ResourceAction, ResourceClass},
};
use perry_hir::{
    ir::{BinaryOp, Expr, Function, Module, Param, Stmt},
    types::{FuncId, LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result};

fn synth_param(id: LocalId, name: String, ty: HirType) -> Param {
    Param {
        id,
        name,
        ty,
        default: None,
        is_rest: false,
        decorators: Vec::new(),
        arguments_object: None,
    }
}

fn synth_call(fid: FuncId, args: Vec<Expr>, byte_offset: u32, byte_end: u32) -> Expr {
    Expr::Call {
        callee: Box::new(Expr::FuncRef(fid)),
        args,
        type_args: Vec::new(),
        byte_offset,
        byte_end,
    }
}

fn synth_function(
    id: FuncId,
    name: String,
    params: Vec<Param>,
    return_type: HirType,
    body: Vec<Stmt>,
    is_async: bool,
) -> Function {
    Function {
        id,
        name,
        type_params: Vec::new(),
        params,
        return_type,
        body,
        is_async,
        is_generator: false,
        is_exported: false,
        captures: Vec::new(),
        decorators: Vec::new(),
        is_strict: true,
        construct_only: false,
        was_unrolled: false,
        was_plain_async: false,
    }
}

struct Registry<'a> {
    resource: &'a ResourceClass<usize>,
    table: LocalId,
    next: LocalId,
}

fn fresh(id: &mut u32) -> Result<u32> {
    *id = id
        .checked_add(1)
        .ok_or_else(|| Diagnostic::internal("HIR ID space exhausted"))?;
    Ok(*id)
}

fn external(name: &str, args: Vec<Expr>, result: HirType, byte_offset: u32, byte_end: u32) -> Expr {
    Expr::Call {
        callee: Box::new(Expr::ExternFuncRef {
            name: name.into(),
            param_types: vec![],
            return_type: result,
        }),
        args,
        type_args: vec![],
        byte_offset,
        byte_end,
    }
}

fn local(id: LocalId, name: &str, ty: HirType, init: Expr, mutable: bool) -> Stmt {
    Stmt::Let {
        id,
        name: name.into(),
        ty,
        init: Some(init),
        mutable,
    }
}

impl Registry<'_> {
    fn get(&self, rep: Expr) -> Expr {
        mark_non_null(Expr::MapGet {
            map: Box::new(Expr::LocalGet(self.table)),
            key: Box::new(rep),
        })
    }

    fn store(&self, body: &mut Vec<Stmt>, id: LocalId, value: Expr) {
        body.push(local(id, "instance", HirType::Any, value, false));
        body.push(Stmt::Expr(Expr::LocalSet(
            self.next,
            Box::new(Expr::Binary {
                op: BinaryOp::Add,
                left: Box::new(Expr::LocalGet(self.next)),
                right: Box::new(Expr::Number(1.0)),
            }),
        )));
        body.push(Stmt::Expr(Expr::MapSet {
            map: Box::new(Expr::LocalGet(self.table)),
            key: Box::new(Expr::LocalGet(self.next)),
            value: Box::new(Expr::LocalGet(id)),
        }));
    }
}

pub(super) fn bind(
    hir: &mut Module,
    classes: &[ResourceClass<usize>],
    calls: &[Option<ClassCall>],
    witnesses: &mut [Witness<'_, FuncId>],
    mut max_local: LocalId,
    mut max_function: FuncId,
) -> Result<()> {
    let mut registries = Vec::with_capacity(classes.len());
    let mut init = Vec::with_capacity(classes.len() * 2);
    for (owner, resource) in classes.iter().enumerate() {
        let c = hir
            .classes
            .get(resource.class)
            .ok_or_else(|| Diagnostic::internal("lost resource class"))?;
        let table = fresh(&mut max_local)?;
        let next = fresh(&mut max_local)?;
        let ctor = calls
            .iter()
            .zip(witnesses.iter_mut())
            .find_map(|(call, w)| {
                call.as_ref().filter(|call| {
                    matches!(call.action, ResourceAction::New) && call.owner == owner
                })?;
                w.signature.as_mut()
            });
        if let Some(signature) = ctor
            && !signature.params.is_empty()
            && c.constructor
                .as_ref()
                .is_some_and(|f| f.params.iter().any(|p| p.ty == HirType::Any))
        {
            signature.registry = Some((table, c.name.clone()));
        }
        let ty = HirType::Generic {
            base: "Map".into(),
            type_args: vec![HirType::Number, HirType::Named(c.name.clone())],
        };
        init.push(local(table, "instances", ty, Expr::MapNew, false));
        init.push(local(
            next,
            "representation",
            HirType::Number,
            Expr::Number(0.0),
            true,
        ));
        registries.push(Registry {
            table,
            next,
            resource,
        });
    }
    init.append(&mut hir.init);
    hir.init = init;
    for (w, call) in witnesses.iter_mut().zip(calls) {
        let Some(sig) = &w.signature else { continue };
        let owner_of = |value: crate::wit::WitValue| {
            registries
                .iter()
                .find(|r| value.resource() == Some(r.resource.resource.index() as u32))
        };
        if call.is_none()
            && !sig.params.iter().any(|&p| owner_of(p).is_some())
            && sig.result.and_then(owner_of).is_none()
        {
            continue;
        }
        let mut params = Vec::with_capacity(sig.params.len());
        let mut args = Vec::with_capacity(sig.params.len());
        for (i, &p) in sig.params.iter().enumerate() {
            let id = fresh(&mut max_local)?;
            let mut value = Expr::LocalGet(id);
            if let Some(table) = owner_of(p) {
                value = table.get(external(
                    &table.resource.representation,
                    vec![value],
                    HirType::Number,
                    0,
                    0,
                ));
            }
            args.push(value);
            params.push(synth_param(id, format!("arg{i}"), HirType::Any));
        }
        let mut body = Vec::new();
        let operation = if let Some(call) = call {
            let table = registries
                .get(call.owner)
                .ok_or_else(|| Diagnostic::internal("missing handle table"))?;
            let class = hir
                .classes
                .get_mut(table.resource.class)
                .ok_or_else(|| Diagnostic::internal("missing class declaration"))?;
            match call.action {
                ResourceAction::New => Expr::New {
                    class_name: class.name.clone(),
                    args,
                    type_args: vec![],
                    byte_offset: 0,
                    byte_end: 0,
                    cap_args_appended: 0,
                },
                ResourceAction::Drop(dispose) => {
                    let rep = params
                        .first()
                        .ok_or_else(|| Diagnostic::internal("missing destructor parameter"))?
                        .id;
                    if let Some(fid) = dispose {
                        let idx = class
                            .computed_members
                            .iter()
                            .position(|m| m.function.id == fid)
                            .ok_or_else(|| Diagnostic::internal("missing disposal method"))?;
                        let m = class.computed_members.remove(idx).function;
                        let name = m.name.clone();
                        class.methods.push(m);
                        hir.init.retain(|s|!matches!(s,Stmt::Expr(Expr::RegisterClassComputedMethod{class_name,method_name,..})
                            if class_name==&class.name && method_name==&name));
                        body.push(Stmt::Expr(method(
                            table.get(Expr::LocalGet(rep)),
                            name,
                            vec![],
                        )));
                    }
                    Expr::MapDelete {
                        map: Box::new(Expr::LocalGet(table.table)),
                        key: Box::new(Expr::LocalGet(rep)),
                    }
                }
                ResourceAction::Static(fid) => {
                    let m = class
                        .static_methods
                        .iter()
                        .find(|m| m.id == fid)
                        .ok_or_else(|| Diagnostic::internal("missing static method"))?;
                    Expr::StaticMethodCall {
                        class_name: class.name.clone(),
                        method_name: m.name.clone(),
                        args,
                    }
                }
                ResourceAction::Call(fid) => {
                    let m = class
                        .methods
                        .iter()
                        .find(|m| m.id == fid)
                        .ok_or_else(|| Diagnostic::internal("missing instance method"))?;
                    let mut args = args.into_iter();
                    let object = args
                        .next()
                        .ok_or_else(|| Diagnostic::internal("missing method receiver"))?;
                    method(object, m.name.clone(), args.collect())
                }
            }
        } else {
            synth_call(
                w.target
                    .ok_or_else(|| Diagnostic::internal("missing resource conversion target"))?,
                args,
                0,
                0,
            )
        };
        if let Some(table) = sig.result.and_then(owner_of) {
            let r = table.resource;
            table.store(&mut body, fresh(&mut max_local)?, operation);
            body.push(Stmt::Return(Some(external(
                &r.create,
                vec![Expr::LocalGet(table.next)],
                HirType::Named(r.source_name.clone()),
                0,
                0,
            ))));
        } else if sig.result.is_none() {
            body.push(Stmt::Expr(operation));
        } else {
            body.push(Stmt::Return(Some(operation)));
        }
        let fid = fresh(&mut max_function)?;
        hir.functions.push(synth_function(
            fid,
            format!("resource_adapter_{fid}"),
            params,
            if sig.result.is_some() {
                HirType::Any
            } else {
                HirType::Void
            },
            body,
            false,
        ));
        w.target = Some(fid);
    }
    Ok(())
}

fn method(receiver: Expr, name: String, args: Vec<Expr>) -> Expr {
    Expr::Call {
        callee: Box::new(Expr::PropertyGet {
            object: Box::new(receiver),
            property: name,
            byte_offset: 0,
            byte_end: 0,
        }),
        args,
        type_args: vec![],
        byte_offset: 0,
        byte_end: 0,
    }
}
