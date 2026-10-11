//! Opaque resource lowering and acknowledgement-scoped canonical ownership changes.
use super::*;
use crate::host::Host;
use pliron::{context::Ptr, operation::Operation, r#type::Typed};
use std::collections::HashSet;

pub(crate) fn used_keys(ctx: &Context, functions: &[FuncOp]) -> HashSet<u32> {
    use pliron::{builtin::op_interfaces::OneRegionInterface, linked_list::ContainsLinkedList};
    fn visit(
        ctx: &Context,
        ty: TypeHandle,
        seen: &mut HashSet<TypeHandle>,
        keys: &mut HashSet<u32>,
    ) {
        if !seen.insert(ty) {
            return;
        }
        let ty = ty.deref(ctx);
        if let Some(resource) = ty.downcast_ref::<perry_mir::types::ResourceType>() {
            keys.insert(resource.key());
        } else if let Some(record) = ty.downcast_ref::<RecordType>() {
            if let Ok(fields) = record.fields() {
                for (_, field) in fields {
                    visit(ctx, *field, seen, keys);
                }
            }
        } else if let Some(array) = ty.downcast_ref::<ArrayType>() {
            visit(ctx, array.element(), seen, keys);
        } else if let Some(optional) = ty.downcast_ref::<perry_mir::types::OptionalType>() {
            visit(ctx, optional.value(), seen, keys);
        } else if let Some(promise) = ty.downcast_ref::<perry_mir::types::PromiseType>() {
            visit(ctx, promise.value(), seen, keys);
        } else if let Some(variant) = ty.downcast_ref::<perry_mir::types::VariantType>() {
            for (_, payload) in variant.cases() {
                if let Some(payload) = payload {
                    visit(ctx, *payload, seen, keys);
                }
            }
        }
    }
    let mut seen = HashSet::new();
    let mut keys = HashSet::new();
    for function in functions {
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            for value in block.deref(ctx).arguments() {
                visit(ctx, value.get_type(ctx), &mut seen, &mut keys);
            }
            for op in block.deref(ctx).iter(ctx) {
                for value in op.deref(ctx).results() {
                    visit(ctx, value.get_type(ctx), &mut seen, &mut keys);
                }
            }
        }
    }
    keys
}

impl AbiType {
    pub(super) fn has_resources(&self) -> bool {
        match self {
            Self::Resource { .. } => true,
            Self::List(value) | Self::Option(value) => value.has_resources(),
            Self::Record(fields) => fields.iter().any(|(_, ty)| ty.has_resources()),
            Self::Result { ok, err } => ok.iter().chain(err).any(|ty| ty.has_resources()),
            Self::Variant(cases) => cases
                .iter()
                .any(|(_, payload)| payload.as_ref().is_some_and(Self::has_resources)),
            _ => false,
        }
    }
}

pub(crate) fn emit_drain(ctx: &mut Context, state: &mut ModuleState) {
    if state.resources.is_empty() {
        return;
    }
    let ty = FunctionType::get(ctx, vec![], vec![]);
    let function = FuncOp::new(ctx, pliron::ident!("perry_resources_drain"), ty);
    let mut b = FunctionLowering::glue(state, ctx, &function);
    let next = b.new_block(ctx, vec![]);
    let dispatch = b.new_block(ctx, vec![]);
    let done = b.new_block(ctx, vec![]);
    b.br(ctx, next, vec![]);
    b.block = next;
    let drop = b.runtime(ctx, Runtime::ResourceDropNext, vec![]);
    let zero = b.i64_const(ctx, 0);
    let present = b.num(ctx, NumOpcode::I64Ne, vec![drop, zero]);
    b.cond_br(ctx, present, (dispatch, vec![]), (done, vec![]));
    b.block = dispatch;
    let handle = b.num(ctx, NumOpcode::I32WrapI64, vec![drop]);
    let shift = b.i64_const(ctx, 32);
    let kind = b.num(ctx, NumOpcode::I64ShrU, vec![drop, shift]);
    let kind = b.num(ctx, NumOpcode::I32WrapI64, vec![kind]);
    let mut resources: Vec<_> = b.state.resources.values().copied().collect();
    resources.sort_by_key(|r| r.key);
    for resource in resources {
        let key = b.i32_const(ctx, resource.key as i32 + 1);
        let matches = b.num(ctx, NumOpcode::I32Eq, vec![kind, key]);
        let selected = b.new_block(ctx, vec![]);
        let other = b.new_block(ctx, vec![]);
        b.cond_br(ctx, matches, (selected, vec![]), (other, vec![]));
        b.block = selected;
        b.resource_drop(ctx, resource.key, handle);
        b.br(ctx, next, vec![]);
        b.block = other;
    }
    let trap = UnreachableOp::new(ctx).get_operation();
    b.insert(ctx, trap);
    b.block = done;
    let ret = ReturnOp::new(ctx, vec![]).get_operation();
    b.insert(ctx, ret);
    b.state
        .wasm
        .append_operation(ctx, function.get_operation(), 0);
}

impl FunctionLowering<'_, '_> {
    pub(super) fn return_resources(&mut self, ctx: &mut Context) {
        if self.state.resources.is_empty() {
            return;
        }
        self.runtime_void(ctx, Runtime::ResourceTaskReturn, vec![]);
        self.runtime_void(ctx, Runtime::GcBoundary, vec![]);
        self.drain_resources(ctx);
    }
    pub(crate) fn drain_resources(&mut self, ctx: &mut Context) {
        if self.state.resources.is_empty() {
            return;
        }
        // This is a generated function, not a runtime helper import.
        let call = CallOp::new(ctx, pliron::ident!("perry_resources_drain"), vec![], vec![])
            .get_operation();
        self.insert(ctx, call);
    }

    /// Call `[resource-new]` or `[resource-rep]` of resource `key`.
    fn resource_value(
        &mut self,
        ctx: &mut Context,
        key: u32,
        operation: &str,
        argument: Value,
    ) -> Value {
        let host = self.resource_builtin(key, operation, vec![Flat::I32]);
        self.preserving_task(ctx, |b, ctx| b.host(ctx, host, vec![argument]))
    }

    /// Call `[resource-drop]` of resource `key`.
    fn resource_drop(&mut self, ctx: &mut Context, key: u32, handle: Value) {
        let host = self.resource_builtin(key, "drop", vec![]);
        self.preserving_task(ctx, |b, ctx| b.host_void(ctx, host, vec![handle]));
    }

    fn resource_builtin(&self, key: u32, operation: &str, results: Vec<Flat>) -> Host {
        let resource = &self.state.resources[&key];
        let module = if resource.exported {
            format!("[export]{}", resource.module)
        } else {
            resource.module.clone()
        };
        Host::Foreign(
            module,
            format!("[resource-{operation}]{}", resource.name),
            vec![Flat::I32],
            results,
        )
    }

    pub(super) fn begin_resources(
        &mut self,
        ctx: &mut Context,
        needed: bool,
        roots: Option<Value>,
    ) {
        assert!(
            self.resource_scope.is_none(),
            "resource scopes are not nested; the previous one must end first"
        );
        if needed {
            let scope = self.runtime(ctx, Runtime::ResourceScopeNew, vec![]);
            self.resource_scope = Some((scope, roots));
        }
    }
    pub(super) fn commit_resources(&mut self, ctx: &mut Context) {
        if let Some((scope, _)) = self.resource_scope {
            self.runtime_void(ctx, Runtime::ResourceScopeCommit, vec![scope]);
        }
    }
    pub(super) fn finish_resources(&mut self, ctx: &mut Context) {
        if let Some((scope, _)) = self.resource_scope.take() {
            self.runtime_void(ctx, Runtime::ResourceScopeFinish, vec![scope]);
            self.drain_resources(ctx);
        }
    }
    fn resource_status(&mut self, ctx: &mut Context) {
        let status = self.runtime(ctx, Runtime::Status, vec![]);
        let failed = self.new_block(ctx, vec![]);
        let normal = self.new_block(ctx, vec![]);
        self.cond_br(ctx, status, (failed, vec![]), (normal, vec![]));
        self.block = failed;
        if let Some((scope, roots)) = self.resource_scope {
            self.runtime_void(ctx, Runtime::ResourceScopeFinish, vec![scope]);
            if let Some(roots) = roots {
                self.reset_roots(ctx, roots);
            }
        }
        self.drain_resources(ctx);
        if self.throws {
            let (target, args) = self.throw_target(ctx, status);
            self.br(ctx, target, args);
        } else {
            self.task_free(ctx);
            let trap = UnreachableOp::new(ctx).get_operation();
            self.insert(ctx, trap);
        }
        self.block = normal;
    }
    fn check_resource_type(&self, ctx: &Context, ty: TypeHandle, key: u32) -> Result<()> {
        ensure!(
            ty.deref(ctx)
                .downcast_ref::<perry_mir::types::ResourceType>()
                .is_some_and(|r| r.key() == key),
            "WIT resource requires its declared opaque SDK type"
        );
        Ok(())
    }
    pub(super) fn lift_resource(
        &mut self,
        ctx: &mut Context,
        key: u32,
        owned: bool,
        ty: TypeHandle,
        handle: Value,
    ) -> Result<Value> {
        self.check_resource_type(ctx, ty, key)?;
        let representation = self.state.resources[&key].exported;
        let mode = self.i32_const(
            ctx,
            if owned {
                0
            } else if representation {
                2
            } else {
                1
            },
        );
        let key = self.i32_const(ctx, key as i32);
        Ok(self.runtime(ctx, Runtime::ResourceNew, vec![key, handle, mode]))
    }
    pub(super) fn lower_resource(
        &mut self,
        ctx: &mut Context,
        key: u32,
        owned: bool,
        ty: TypeHandle,
        value: Value,
    ) -> Result<Value> {
        self.check_resource_type(ctx, ty, key)?;
        let (scope, _) = self.resource_scope.expect("canonical resource scope");
        let own = self.i32_const(ctx, i32::from(owned));
        let handle = self.runtime(ctx, Runtime::ResourceLower, vec![scope, value, own]);
        self.resource_status(ctx);
        Ok(handle)
    }

    pub(super) fn resource_helper(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        key: u32,
        operation: ResourceOperation,
    ) -> Result<()> {
        let value = self.operand(ctx, op, 0)?;
        match operation {
            ResourceOperation::New => {
                let handle = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![value]);
                let handle = self.resource_value(ctx, key, "new", handle);
                let ty = op.deref(ctx).get_result(0).get_type(ctx);
                let value = self.lift_resource(ctx, key, true, ty, handle)?;
                self.define(ctx, op, value);
            }
            ResourceOperation::Drop => {
                self.runtime_void(ctx, Runtime::ResourceDrop, vec![value]);
                self.resource_status(ctx);
                self.drain_resources(ctx);
            }
            ResourceOperation::Representation => {
                let handle = self.runtime(ctx, Runtime::ResourceHandle, vec![value]);
                self.resource_status(ctx);
                let rep = self.runtime(ctx, Runtime::ResourceIsRep, vec![value]);
                let lookup = self.new_block(ctx, vec![]);
                let i32 = i32_type(ctx);
                let done = self.new_block(ctx, vec![i32]);
                self.cond_br(ctx, rep, (done, vec![handle]), (lookup, vec![]));
                self.block = lookup;
                let representation = self.resource_value(ctx, key, "rep", handle);
                self.br(ctx, done, vec![representation]);
                self.block = done;
                let value = done.deref(ctx).get_argument(0);
                let value = self.num(ctx, NumOpcode::F64ConvertI32U, vec![value]);
                self.define(ctx, op, value);
            }
        }
        Ok(())
    }
    pub(super) fn discard_resource(&mut self, ctx: &mut Context, key: u32, raw: Value) {
        self.resource_drop(ctx, key, raw);
    }
}
