//! Return and throw completions become canonical result cases at the export boundary.

use super::*;

pub(super) struct Completion {
    failed: Value,
    payload: Value,
}

impl Adapter<'_> {
    pub(super) fn retain_completion(
        &mut self,
        result: &wit_parser::Result_,
        source: &HirType,
        payload: Value,
        rejection: Block,
    ) -> Result<Completion> {
        let join = self.body.add_block();
        let completion = Completion {
            failed: self.body.add_blockparam(join, CoreType::I32),
            payload: self.body.add_blockparam(join, CoreType::F64),
        };
        if crate::waffle_backend::ssa::types::is_reference(source)
            && let Some(ty) = result.ok
        {
            let value = self.decode(ty, payload)?;
            self.scratch
                .as_ref()
                .unwrap()
                .retain(&mut self.body, self.block, value);
        }
        let returned = self.integer(0);
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: join,
                    args: vec![returned, payload],
                },
            },
        );

        self.block = rejection;
        let status = self.body.blocks[rejection].params[0].1;
        let mut payload = self.body.blocks[rejection].params[1].1;
        if result.err.is_some() {
            payload = self.op(
                Operator::Call {
                    function_index: self.registry.errors.unwrap().normalize,
                },
                &[status, payload],
                CoreType::F64,
            );
            let boxed = abi::decode_payload(&mut self.body, self.block, payload, true);
            self.scratch
                .as_ref()
                .unwrap()
                .retain(&mut self.body, self.block, boxed);
        }
        let threw = self.integer(1);
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: join,
                    args: vec![threw, payload],
                },
            },
        );
        self.block = join;
        Ok(completion)
    }

    pub(super) fn lower_completion_result(
        &mut self,
        ty: Type,
        result: &wit_parser::Result_,
        source: &HirType,
        completion: Completion,
    ) -> Result<Value> {
        let offset = self
            .sizes
            .payload_offset(Int::U8, [result.ok.as_ref(), result.err.as_ref()])
            .size_wasm32() as u32;
        let address = self.allocate_canonical_result(ty);
        self.body.add_op(
            self.block,
            Operator::I32Store8 {
                memory: self.memory(0),
            },
            &[address, completion.failed],
            &[],
        );
        let success = self.body.add_block();
        let failure = self.body.add_block();
        let join = self.body.add_block();
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: completion.failed,
                if_true: BlockTarget {
                    block: failure,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: success,
                    args: vec![],
                },
            },
        );
        self.block = success;
        if let Some(ok) = result.ok {
            let value = self.decode(ok, completion.payload)?;
            self.lower(ok, value, address, offset, source)?;
        }
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: join,
                    args: vec![],
                },
            },
        );
        self.block = failure;
        if let Some(err) = result.err {
            let boxed = abi::decode_payload(&mut self.body, self.block, completion.payload, true);
            let value = self.extract(err, boxed)?;
            self.lower(
                err,
                value,
                address,
                offset,
                &super::super::hir_type(&self.wit.resolve, err)?,
            )?;
        }
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: join,
                    args: vec![],
                },
            },
        );
        self.block = join;
        Ok(address)
    }
}
