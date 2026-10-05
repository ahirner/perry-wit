//! Typed construction of guest runtime functions sharing the canonical heap.

use anyhow::Result;
use waffle::{
    Block, BlockTarget, Func, FuncDecl, FunctionBody, Import, ImportKind, Memory, MemoryArg,
    Module, Operator, SignatureData, Terminator, Type, Value, ValueDef,
};

pub(crate) fn declare(
    module: &mut Module<'static>,
    name: &str,
    params: &[Type],
    returns: &[Type],
) -> Func {
    let sig = module.signatures.push(SignatureData {
        params: params.to_vec(),
        returns: returns.to_vec(),
    });
    let body = FunctionBody::new(module, sig);
    module.funcs.push(FuncDecl::Body(sig, name.into(), body))
}

pub(crate) fn native(
    module: &mut Module<'static>,
    name: &str,
    params: &[Type],
    returns: &[Type],
) -> Func {
    let sig = module.signatures.push(SignatureData {
        params: params.to_vec(),
        returns: returns.to_vec(),
    });
    let function = module.funcs.push(FuncDecl::Import(sig, name.into()));
    module.imports.push(Import {
        module: "$root".into(),
        name: name.into(),
        kind: ImportKind::Func(function),
    });
    function
}

pub(crate) struct Builder {
    pub(crate) body: FunctionBody,
    pub(crate) block: Block,
    memory: Memory,
}

impl Builder {
    pub(crate) fn new(module: &Module<'static>, function: Func, memory: Memory) -> Self {
        let body = FunctionBody::new(module, module.funcs[function].sig());
        Self {
            block: body.entry,
            body,
            memory,
        }
    }
    pub(crate) fn param(&self, index: usize) -> Value {
        self.body.blocks[self.body.entry].params[index].1
    }
    pub(crate) fn op(&mut self, op: Operator, args: &[Value], ty: Type) -> Value {
        self.body.add_op(self.block, op, args, &[ty])
    }
    pub(crate) fn effect(&mut self, op: Operator, args: &[Value]) {
        self.body.add_op(self.block, op, args, &[]);
    }
    pub(crate) fn integer(&mut self, value: u32) -> Value {
        self.op(Operator::I32Const { value }, &[], Type::I32)
    }
    pub(crate) fn number(&mut self, value: f64) -> Value {
        self.op(
            Operator::F64Const {
                value: value.to_bits(),
            },
            &[],
            Type::F64,
        )
    }
    pub(crate) fn memory(&self, offset: u32) -> MemoryArg {
        MemoryArg {
            memory: self.memory,
            offset,
            align: 0,
        }
    }
    pub(crate) fn load(&mut self, address: Value, offset: u32, ty: Type) -> Value {
        let memory = self.memory(offset);
        let op = match ty {
            Type::I32 => Operator::I32Load { memory },
            Type::F64 => Operator::F64Load { memory },
            Type::I64 => Operator::I64Load { memory },
            Type::F32 => Operator::F32Load { memory },
            _ => unreachable!(),
        };
        self.op(op, &[address], ty)
    }
    pub(crate) fn store(&mut self, address: Value, offset: u32, value: Value, ty: Type) {
        let memory = self.memory(offset);
        let op = match ty {
            Type::I32 => Operator::I32Store { memory },
            Type::F64 => Operator::F64Store { memory },
            Type::I64 => Operator::I64Store { memory },
            Type::F32 => Operator::F32Store { memory },
            _ => unreachable!(),
        };
        self.effect(op, &[address, value]);
    }
    pub(crate) fn call(
        &mut self,
        function_index: Func,
        args: &[Value],
        results: &[Type],
    ) -> Vec<Value> {
        let value = self
            .body
            .add_op(self.block, Operator::Call { function_index }, args, results);
        if results.len() == 1 {
            return vec![value];
        }
        results
            .iter()
            .enumerate()
            .map(|(index, ty)| {
                let result = self
                    .body
                    .add_value(ValueDef::PickOutput(value, index as u32, *ty));
                self.body.append_to_block(self.block, result);
                result
            })
            .collect()
    }
    pub(crate) fn allocate(&mut self, realloc: Func, size: u32, alignment: u32) -> Value {
        let zero = self.integer(0);
        let alignment = self.integer(alignment);
        let size = self.integer(size);
        self.call(realloc, &[zero, zero, alignment, size], &[Type::I32])[0]
    }
    pub(crate) fn jump(&mut self, block: Block, args: &[Value]) {
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block,
                    args: args.to_vec(),
                },
            },
        );
    }
    pub(crate) fn branch(&mut self, condition: Value, yes: Block, no: Block) {
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: condition,
                if_true: BlockTarget {
                    block: yes,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: no,
                    args: vec![],
                },
            },
        );
    }
    pub(crate) fn require(&mut self, condition: Value) {
        let next = self.body.add_block();
        let trap = self.body.add_block();
        self.branch(condition, next, trap);
        self.body.set_terminator(trap, Terminator::Unreachable);
        self.block = next;
    }
    pub(crate) fn ret(&mut self, values: &[Value]) {
        self.body.set_terminator(
            self.block,
            Terminator::Return {
                values: values.to_vec(),
            },
        );
    }
    pub(crate) fn finish(self, module: &mut Module<'static>, function: Func) -> Result<()> {
        self.body.validate()?;
        self.body.verify_reducible()?;
        let sig = module.funcs[function].sig();
        let name = module.funcs[function].name().to_string();
        module.funcs[function] = FuncDecl::Body(sig, name, self.body);
        Ok(())
    }
}
