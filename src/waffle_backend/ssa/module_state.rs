//! Module bindings are shared storage, independent of function-local SSA joins.

use anyhow::Result;
use perry_hir::{ir::Expr, types::LocalId};
use waffle::{BlockTarget, MemoryArg, Operator, Terminator, Type, Value};

use super::FunctionLowerer;
use crate::waffle_backend::initialization::Binding;

impl FunctionLowerer<'_> {
    pub(super) fn module_binding(&self, id: LocalId) -> Option<Binding> {
        self.registry
            .module_state
            .as_ref()?
            .bindings
            .get(&id)
            .cloned()
    }

    pub(super) fn read_module_binding(&mut self, binding: &Binding) -> Value {
        let initialized = self.op(
            Operator::GlobalGet {
                global_index: binding.initialized,
            },
            &[],
            &[Type::I32],
        );
        let ready = self.body.add_block();
        let uninitialized = self.body.add_block();
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: initialized,
                if_true: BlockTarget {
                    block: ready,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: uninitialized,
                    args: vec![],
                },
            },
        );
        self.block = uninitialized;
        let error = self.op(
            Operator::F64Const {
                value: 1f64.to_bits(),
            },
            &[],
            &[Type::F64],
        );
        self.emit_native_throw(error);
        self.block = ready;
        let ty = self.module.globals[binding.value].ty;
        self.op(
            Operator::GlobalGet {
                global_index: binding.value,
            },
            &[],
            &[ty],
        )
    }

    pub(super) fn assign_module_binding(
        &mut self,
        binding: &Binding,
        expression: &Expr,
    ) -> Result<Value> {
        let value = self.typed_operand(expression, &binding.ty)?;
        self.write_module_binding(binding, value);
        Ok(value)
    }

    pub(super) fn write_module_binding(&mut self, binding: &Binding, value: Value) {
        self.body.add_op(
            self.block,
            Operator::GlobalSet {
                global_index: binding.value,
            },
            &[value],
            &[],
        );
        let initialized = self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]);
        self.body.add_op(
            self.block,
            Operator::GlobalSet {
                global_index: binding.initialized,
            },
            &[initialized],
            &[],
        );
        if let Some(slot) = binding.root {
            let roots = self.op(
                Operator::GlobalGet {
                    global_index: self.registry.module_state.as_ref().unwrap().roots,
                },
                &[],
                &[Type::I32],
            );
            self.body.add_op(
                self.block,
                Operator::I32Store {
                    memory: MemoryArg {
                        memory: self.registry.memory,
                        offset: 12 + slot * 4,
                        align: 2,
                    },
                },
                &[roots, value],
                &[],
            );
        }
    }
}
