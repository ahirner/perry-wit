//! Resource values distinguish live ownership, scoped borrows, and consumed aliases.
use super::super::resources::{STATE, handle_id, key};
use super::*;
use wit_parser::{Handle, TypeId};

impl Adapter<'_> {
    pub(super) fn initialize_resources(&mut self) {
        if self.registry.resource_functions.is_empty() {
            return;
        }
        let head = self.allocate(4, 4);
        let zero = self.integer(0);
        self.store_i32(head, 0, zero);
        self.borrowed = Some(head);
    }
    fn resource_state(&mut self, object: Value) -> Result<Value> {
        let state = self.read_field(object, STATE, Type::U32)?;
        let zero = self.number(0.0);
        let alive = self.op(Operator::F64Gt, &[state, zero], CoreType::I32);
        self.require(alive);
        Ok(state)
    }
    fn raw_resource(&mut self, id: TypeId, object: Value) -> Result<Value> {
        let raw = self.read_field(object, &key(id), Type::U32)?;
        Ok(self.op(Operator::I32TruncF64U, &[raw], CoreType::I32))
    }
    fn invalidate_resource(&mut self, object: Value) -> Result<()> {
        let zero = self.number(0.0);
        self.set_field(object, STATE, Type::U32, zero)
    }
    pub(super) fn lift_resource(
        &mut self,
        ty: Type,
        handle: Handle,
        source: &mut Input<'_>,
    ) -> Result<Value> {
        let id = handle_id(&self.wit.resolve, handle);
        let raw = self.read_scalar(ty, source)?;
        let object = self.call(self.registry.object_helpers.unwrap().new, &[]);
        let number = self.op(Operator::F64ConvertI32U, &[raw], CoreType::F64);
        self.set_field(object, &key(id), Type::U32, number)?;
        let borrowed = matches!(handle, Handle::Borrow(_));
        let representation = borrowed && self.exporting && self.wit.resources[&id].exported;
        let state = self.number(if representation {
            3.0
        } else if borrowed {
            2.0
        } else {
            1.0
        });
        self.set_field(object, STATE, Type::U32, state)?;
        if borrowed {
            self.defer_resource(id, raw, object, !representation);
        }
        Ok(object)
    }
    pub(super) fn lower_resource(&mut self, handle: Handle, object: Value) -> Result<Value> {
        let id = handle_id(&self.wit.resolve, handle);
        let state = self.resource_state(object)?;
        let raw = self.raw_resource(id, object)?;
        if matches!(handle, Handle::Own(_)) {
            let one = self.number(1.0);
            let owned = self.op(Operator::F64Eq, &[state, one], CoreType::I32);
            self.require(owned);
            self.invalidate_resource(object)?;
            return Ok(raw);
        }
        let Some(new) = self.registry.resource_functions[&id].new else {
            return Ok(raw);
        };
        let three = self.number(3.0);
        let representation = self.op(Operator::F64Eq, &[state, three], CoreType::I32);
        let create = self.body.add_block();
        let direct = self.body.add_block();
        let join = self.body.add_block();
        let result = self.body.add_blockparam(join, CoreType::I32);
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: representation,
                if_true: BlockTarget {
                    block: create,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: direct,
                    args: vec![],
                },
            },
        );
        self.block = create;
        let temporary = self.call(new, &[raw]);
        let zero = self.integer(0);
        self.defer_resource(id, temporary, zero, true);
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: join,
                    args: vec![temporary],
                },
            },
        );
        self.body.set_terminator(
            direct,
            Terminator::Br {
                target: BlockTarget {
                    block: join,
                    args: vec![raw],
                },
            },
        );
        self.block = join;
        Ok(result)
    }
    pub(super) fn resource_rep(&mut self, id: TypeId, object: Value) -> Result<Value> {
        let state = self.resource_state(object)?;
        let raw = self.raw_resource(id, object)?;
        let three = self.number(3.0);
        let representation = self.op(Operator::F64Eq, &[state, three], CoreType::I32);
        let lookup = self.body.add_block();
        let join = self.body.add_block();
        let result = self.body.add_blockparam(join, CoreType::I32);
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: representation,
                if_true: BlockTarget {
                    block: join,
                    args: vec![raw],
                },
                if_false: BlockTarget {
                    block: lookup,
                    args: vec![],
                },
            },
        );
        self.block = lookup;
        let rep = self.call(self.registry.resource_functions[&id].rep.unwrap(), &[raw]);
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: join,
                    args: vec![rep],
                },
            },
        );
        self.block = join;
        Ok(result)
    }
    fn defer_resource(&mut self, id: TypeId, raw: Value, object: Value, drop: bool) {
        let head = self.borrowed.unwrap();
        let next = self.load_i32(head, 0);
        let node = self.allocate(20, 4);
        let resource = self.integer(id.index() as u32);
        let drop = self.integer(u32::from(drop));
        for (offset, value) in [(0, next), (4, resource), (8, raw), (12, object), (16, drop)] {
            self.store_i32(node, offset, value);
        }
        self.store_i32(head, 0, node);
        self.scratch
            .as_ref()
            .unwrap()
            .retain(&mut self.body, self.block, object);
    }
    pub(super) fn finish_resources(&mut self) -> Result<()> {
        let Some(head) = self.borrowed else {
            return Ok(());
        };
        let start = self.load_i32(head, 0);
        let check = self.body.add_block();
        let body = self.body.add_block();
        let done = self.body.add_block();
        let node = self.body.add_blockparam(check, CoreType::I32);
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: check,
                    args: vec![start],
                },
            },
        );
        self.body.set_terminator(
            check,
            Terminator::CondBr {
                cond: node,
                if_true: BlockTarget {
                    block: body,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: done,
                    args: vec![],
                },
            },
        );
        self.block = body;
        let object = self.load_i32(node, 12);
        let invalidate = self.body.add_block();
        let disposed = self.body.add_block();
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: object,
                if_true: BlockTarget {
                    block: invalidate,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: disposed,
                    args: vec![],
                },
            },
        );
        self.block = invalidate;
        self.invalidate_resource(object)?;
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: disposed,
                    args: vec![],
                },
            },
        );
        self.block = disposed;
        let dropping = self.load_i32(node, 16);
        let drop = self.body.add_block();
        let advance = self.body.add_block();
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: dropping,
                if_true: BlockTarget {
                    block: drop,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: advance,
                    args: vec![],
                },
            },
        );
        self.block = drop;
        let resource = self.load_i32(node, 4);
        let raw = self.load_i32(node, 8);
        for (id, functions) in &self.registry.resource_functions {
            let expected = self.integer(id.index() as u32);
            let matches = self.op(Operator::I32Eq, &[resource, expected], CoreType::I32);
            let selected = self.body.add_block();
            let next = self.body.add_block();
            self.body.set_terminator(
                self.block,
                Terminator::CondBr {
                    cond: matches,
                    if_true: BlockTarget {
                        block: selected,
                        args: vec![],
                    },
                    if_false: BlockTarget {
                        block: next,
                        args: vec![],
                    },
                },
            );
            self.body.add_op(
                selected,
                Operator::Call {
                    function_index: functions.drop,
                },
                &[raw],
                &[],
            );
            self.body.set_terminator(
                selected,
                Terminator::Br {
                    target: BlockTarget {
                        block: advance,
                        args: vec![],
                    },
                },
            );
            self.block = next;
        }
        self.body
            .set_terminator(self.block, Terminator::Unreachable);
        self.block = advance;
        let next = self.load_i32(node, 0);
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: check,
                    args: vec![next],
                },
            },
        );
        self.block = done;
        let zero = self.integer(0);
        self.store_i32(head, 0, zero);
        Ok(())
    }
}
