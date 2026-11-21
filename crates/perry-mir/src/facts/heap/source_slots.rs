//! Source representations on the same allocation cells used for numeric layouts.
use super::*;
use std::collections::HashSet;

impl Solver {
    pub(super) fn source_local(&mut self, ctx: &Context, id: usize, value: Value) -> Result<usize> {
        if let Some(&cell) = self.analysis.states[id].heap_values.get(&value) {
            return Ok(cell);
        }
        let slot = TypedHandle::<crate::SlotType>::from_handle(value.get_type(ctx), ctx)?;
        let ty = slot.deref(ctx).value();
        let cell = if let Some(name) = global(ctx, value) {
            self.global_cell(name, ty)
        } else {
            self.heap_cell(ty, None)
        };
        self.analysis.states[id].heap_values.insert(value, cell);
        // An absent value has no allocation to carry the declared shared layout.
        if crate::optional::absence(ctx, ty).is_some()
            && aggregate(ctx, ty)
            && self.analysis.heap.cells[cell].layout_objects.is_empty()
        {
            let origin = self.heap_object(ctx, ty, false)?;
            self.add_object(ctx, cell, origin)?;
        }
        Ok(cell)
    }

    pub(super) fn require_source_slots(
        &mut self,
        ctx: &Context,
        cell: usize,
        ty: TypeHandle,
        reader: Work,
        seen: &mut HashSet<(usize, TypeHandle)>,
    ) -> Result<()> {
        let ty = payload(ctx, ty);
        if !seen.insert((cell, ty)) {
            return Ok(());
        }
        if crate::is_byte_input(ctx, ty) {
            self.update_slot(ctx, cell, ValueFacts::tagged_source());
        } else if aggregate(ctx, ty) {
            self.analysis.heap.cells[cell].readers.insert(reader);
            let mut previous = None;
            while let Some(object) = self.next_layout_object(cell, previous) {
                previous = Some(object);
                for index in 0..self.analysis.heap.objects[object].contents.slots().len() {
                    let slot = self.analysis.heap.objects[object].contents.slots()[index];
                    let t = ty.deref(ctx);
                    let field = if let Some(array) = t.downcast_ref::<ArrayType>() {
                        array.element()
                    } else {
                        let rec = t.downcast_ref::<RecordType>().unwrap();
                        let Some(&(_, field)) = rec.fields()?.get(index) else {
                            continue;
                        };
                        field
                    };
                    self.require_source_slots(ctx, slot, field, reader, seen)?;
                }
            }
        }
        Ok(())
    }
}

pub enum SourceBinding<'a> {
    Local(Value),
    Result(&'a Identifier, TypeHandle),
}

impl ProgramFacts {
    /// Project a source binding onto its shared writable layout, preserving nominal record identities.
    pub fn source_layout(
        &self,
        ctx: &Context,
        binding: SourceBinding<'_>,
        record: &mut dyn FnMut(TypeHandle, TypeHandle),
    ) -> Result<Option<TypeHandle>> {
        let (ty, cell) = match binding {
            SourceBinding::Local(value) => {
                let Some(function) = value
                    .get_parent_block(ctx)
                    .and_then(|block| block.deref(ctx).get_parent_op(ctx))
                else {
                    return Ok(None);
                };
                let Some(&cell) = self.states[self.generic[&function]].heap_values.get(&value)
                else {
                    return Ok(None);
                };
                (self.heap.cells[cell].ty, cell)
            }
            SourceBinding::Result(name, ty) => {
                let Some(result) = self
                    .generic_context(name)
                    .and_then(|id| self.states[id].results.first())
                else {
                    return Ok(None);
                };
                let Some(cell) = result.heap else {
                    return Ok(result
                        .facts
                        .source_tag
                        .then(|| specialize::tagged_type(ctx, ty))
                        .flatten());
                };
                (ty, cell)
            }
        };
        self.cell_layout(ctx, ty, cell, specialize::LayoutRule::Source, record)
    }
}
