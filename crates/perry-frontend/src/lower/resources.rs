//! Canonical handles retain guest class instances until their destructor runs.
use super::*;
use crate::engine::types::ClassInstId;
use crate::hir_compat::ResourceClass;
use crate::hir_compat::Target;

impl<'s> FunctionBuilder<'s, '_> {
    fn guest_class(&self, owner: u32) -> Result<&'s ResourceClass<ClassInstId>> {
        self.shapes
            .specialization
            .resources
            .get(&owner)
            .ok_or_else(|| internal!("missing resource class"))
    }
    fn resource_table(&mut self, ctx: &mut Context, owner: u32) -> Result<(Value, TypeHandle)> {
        let id = self.guest_class(owner)?.class;
        let value = self
            .shapes
            .resolved_binding(self.shapes.specialization.bodies.classes[id].ty)?
            .carrier;
        let map = self
            .shapes
            .map_box_type(ctx, FP64Type::get(ctx).into(), value)?;
        Ok((
            self.named_global_slot(ctx, format!("resource_instances_{owner}"), map.into())?,
            value,
        ))
    }

    pub(super) fn initialize_resources(&mut self, ctx: &mut Context) -> Result<()> {
        for &owner in self.shapes.specialization.resources.keys() {
            let (slot, _) = self.resource_table(ctx, owner)?;
            let ty = self.slot_type(ctx, slot)?;
            let table = self.empty_map(ctx, Some(ty))?;
            self.store(ctx, slot, table);
        }
        Ok(())
    }

    fn resource_instance(&mut self, ctx: &mut Context, owner: u32, rep: Value) -> Result<Value> {
        let (slot, ty) = self.resource_table(ctx, owner)?;
        let table = self.load(ctx, slot)?;
        let value = self.kv_get(ctx, table, rep, FP64Type::get(ctx).into(), ty)?;
        self.defined(ctx, value)
    }

    fn resource_conversion(
        &mut self,
        ctx: &mut Context,
        name: &str,
        value: Value,
        result: TypeHandle,
    ) -> Value {
        let op =
            perry_mir::ForeignOp::new(ctx, perry_mir::Phase::Call, name, vec![value], vec![result]);
        self.append(ctx, &op);
        op.get_operation().deref(ctx).get_result(0)
    }

    pub(super) fn resource_adapter(
        &mut self,
        ctx: &mut Context,
        target: Target<FuncId>,
        mut args: Vec<Value>,
    ) -> Result<()> {
        let number = FP64Type::get(ctx).into();
        let resources = &self.shapes.specialization.resources;
        let registry = |ctx: &Context, ty| {
            resources.get_key_value(
                &perry_mir::typed::<perry_mir::ResourceType>(ctx, ty)?
                    .deref(ctx)
                    .key(),
            )
        };
        for arg in &mut args {
            if let Some((&owner, resource)) = registry(ctx, arg.get_type(ctx)) {
                let rep =
                    self.resource_conversion(ctx, &resource.binding.representation, *arg, number);
                *arg = self.resource_instance(ctx, owner, rep)?;
            }
        }
        let result = match target {
            Target::Function(id) | Target::Static(_, id) | Target::Method(_, id) => {
                self.call_values(ctx, id, args.into_iter())?
            }
            Target::Constructor(owner) => {
                let id = self.guest_class(owner)?.class;
                self.call_values(
                    ctx,
                    self.shapes.specialization.bodies.classes[id].factory()?,
                    args.into_iter(),
                )?
            }
            Target::Destructor(owner, id) => {
                let &[rep] = args.as_slice() else {
                    bail_internal!("missing destructor representation")
                };
                if let Some(id) = id {
                    let receiver = self.resource_instance(ctx, owner, rep)?;
                    self.call_values(ctx, id, std::iter::once(receiver))?;
                }
                let (slot, _) = self.resource_table(ctx, owner)?;
                let table = self.load(ctx, slot)?;
                self.kv_delete(ctx, table, rep, number)?;
                None
            }
        };
        let value = match (self.result, result) {
            (None, _) => None,
            (Some(binding), Some(value)) => Some(
                if let Some((&owner, resource)) = registry(ctx, binding.carrier) {
                    let (slot, _) = self.resource_table(ctx, owner)?;
                    let table = self.load(ctx, slot)?;
                    let rep = self.insert_resource(ctx, table, value)?;
                    self.resource_conversion(ctx, &resource.binding.create, rep, binding.carrier)
                } else {
                    self.fit(ctx, value, binding.carrier)?
                },
            ),
            _ => bail_diag!(
                codes::CONTRACT_MISMATCH,
                "resource export must return a value"
            ),
        };
        self.emit(ctx, |ctx| ReturnOp::new(ctx, value.into_iter().collect()));
        Ok(())
    }
}
