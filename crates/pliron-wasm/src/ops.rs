//! Value-level ops of the `wasm` dialect: numerics, memory, calls and imports.
use crate::{
    attrs::{Effect, MemKind, NumOpcode},
    types::{I32_BITS, integer_attr, pliron_type, val_type},
};
use pliron::{
    builtin::{
        attributes::{IdentifierAttr, IntegerAttr, StringAttr, TypeAttr},
        op_interfaces::{
            NOpdsInterface, NRegionsInterface, NResultsInterface, OneResultInterface,
            SymbolOpInterface, SymbolTableInterface,
        },
        ops::FuncOp,
        type_interfaces::FunctionTypeInterface,
        types::FunctionType,
    },
    combine::{Parser, parser::char::string, token},
    common_traits::Verify,
    context::{Context, Ptr},
    derive::{op_interface_impl, pliron_op},
    identifier::Identifier,
    input_err,
    irfmt::{
        parsers::{spaced, type_parser},
        printers::op::typed_symb_op_header,
    },
    location::Located,
    op::{Op, OpObj, op_cast},
    operation::Operation,
    opts::dce::SideEffects,
    parsable::{Parsable, ParseResult, StateStream},
    printable::Printable,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::APInt,
    value::Value,
    verify_err,
};
use thiserror::Error;
use wasm_encoder::ValType;

/// Verification failures of the value-level ops.
#[derive(Debug, Error)]
enum OpErr {
    #[error("{0} operand or result types do not match its signature")]
    Signature(String),
    #[error("callee {0} is neither a function nor an import of the enclosing module")]
    UnknownCallee(String),
    #[error("{0} uses a type that is not a Wasm value type")]
    ValueType(String),
}

fn types_of(values: impl Iterator<Item = Value>, ctx: &Context) -> Option<Vec<ValType>> {
    values.map(|v| val_type(ctx, v.get_type(ctx))).collect()
}

macro_rules! effect_free {
    ($($op:ty),*) => {$(
        #[op_interface_impl]
        impl SideEffects for $op {
            fn has_side_effects(&self, _ctx: &Context) -> bool {
                false
            }
        }
    )*};
}

/// A numeric instruction (`i32.add`, `f64.lt`, `i64.extend_i32_u`, …) on SSA operands.
#[pliron_op(
    name = "wasm.num",
    format = "attr($wasm_num, $NumOpcode) ` ` operands(CharSpace(`,`)) ` : ` type($0)",
    interfaces = [OneResultInterface],
    attributes = (wasm_num: NumOpcode)
)]
pub struct NumOp;

impl NumOp {
    pub fn new(ctx: &mut Context, opcode: NumOpcode, operands: Vec<Value>) -> Self {
        let result = pliron_type(ctx, opcode.signature().1);
        let op = NumOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                operands,
                vec![],
                0,
            ),
        };
        op.set_attr_wasm_num(ctx, opcode);
        op
    }

    pub(crate) fn opcode(&self, ctx: &Context) -> NumOpcode {
        *self
            .get_attr_wasm_num(ctx)
            .expect("wasm.num gets its opcode from NumOp::new or its parsed format")
    }
}

impl Verify for NumOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let opcode = self.opcode(ctx);
        let (operands, result) = opcode.signature();
        let op = self.get_operation().deref(ctx);
        if types_of(op.operands(), ctx).as_deref() != Some(operands)
            || val_type(ctx, self.get_result(ctx).get_type(ctx)) != Some(result)
        {
            verify_err!(
                self.loc(ctx),
                OpErr::Signature(format!("wasm.num {opcode:?}"))
            )?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for NumOp {
    fn has_side_effects(&self, ctx: &Context) -> bool {
        self.opcode(ctx).effect() == Effect::Traps
    }
}

/// `select`: the first value if the `i32` condition is non-zero, else the second.
#[pliron_op(
    name = "wasm.select",
    format = "$2 ` ? ` $0 ` : ` $1 ` : ` type($0)",
    interfaces = [OneResultInterface, NOpdsInterface<3>]
)]
pub struct SelectOp;

impl SelectOp {
    pub fn new(ctx: &mut Context, condition: Value, then: Value, otherwise: Value) -> Self {
        let ty = then.get_type(ctx);
        SelectOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![then, otherwise, condition],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for SelectOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        let (a, b, c) = (op.get_operand(0), op.get_operand(1), op.get_operand(2));
        let result = self.get_result(ctx).get_type(ctx);
        if a.get_type(ctx) != b.get_type(ctx)
            || a.get_type(ctx) != result
            || val_type(ctx, result).is_none()
            || val_type(ctx, c.get_type(ctx)) != Some(ValType::I32)
        {
            verify_err!(self.loc(ctx), OpErr::Signature("wasm.select".into()))?
        }
        Ok(())
    }
}

effect_free!(SelectOp);

fn offset_attr(ctx: &mut Context, offset: u32) -> IntegerAttr {
    integer_attr(ctx, I32_BITS, offset.into())
}

/// The `u32` offset of a memory access.
fn offset_of(attr: &IntegerAttr) -> u32 {
    let value: APInt = attr.clone().into();
    value.to_u64() as u32
}

/// A linear-memory load from `address + offset` in memory 0, naturally aligned.
#[pliron_op(
    name = "wasm.load",
    format = "attr($wasm_load_kind, $MemKind) ` ` $0 ` + ` attr($wasm_load_offset, $IntegerAttr) ` : ` type($0)",
    interfaces = [OneResultInterface, NOpdsInterface<1>],
    attributes = (wasm_load_kind: MemKind, wasm_load_offset: IntegerAttr)
)]
pub struct LoadOp;

impl LoadOp {
    pub fn new(ctx: &mut Context, kind: MemKind, address: Value, offset: u32) -> Self {
        let ty = pliron_type(ctx, kind.value());
        let op = LoadOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![address],
                vec![],
                0,
            ),
        };
        op.set_attr_wasm_load_kind(ctx, kind);
        let offset = offset_attr(ctx, offset);
        op.set_attr_wasm_load_offset(ctx, offset);
        op
    }

    pub(crate) fn kind(&self, ctx: &Context) -> MemKind {
        *self
            .get_attr_wasm_load_kind(ctx)
            .expect("wasm.load gets its kind from LoadOp::new or its parsed format")
    }

    pub(crate) fn offset(&self, ctx: &Context) -> u32 {
        offset_of(
            &self
                .get_attr_wasm_load_offset(ctx)
                .expect("wasm.load gets its offset from LoadOp::new or its parsed format"),
        )
    }
}

impl Verify for LoadOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let address = self.get_operation().deref(ctx).get_operand(0);
        if val_type(ctx, address.get_type(ctx)) != Some(ValType::I32)
            || val_type(ctx, self.get_result(ctx).get_type(ctx)) != Some(self.kind(ctx).value())
        {
            verify_err!(self.loc(ctx), OpErr::Signature("wasm.load".into()))?
        }
        Ok(())
    }
}

/// A linear-memory store of `value` at `address + offset` in memory 0, naturally aligned.
#[pliron_op(
    name = "wasm.store",
    format = "attr($wasm_store_kind, $MemKind) ` ` $0 ` + ` attr($wasm_store_offset, $IntegerAttr) ` = ` $1",
    interfaces = [NResultsInterface<0>, NOpdsInterface<2>],
    attributes = (wasm_store_kind: MemKind, wasm_store_offset: IntegerAttr)
)]
pub struct StoreOp;

impl StoreOp {
    pub fn new(
        ctx: &mut Context,
        kind: MemKind,
        address: Value,
        offset: u32,
        value: Value,
    ) -> Self {
        let op = StoreOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![],
                vec![address, value],
                vec![],
                0,
            ),
        };
        op.set_attr_wasm_store_kind(ctx, kind);
        let offset = offset_attr(ctx, offset);
        op.set_attr_wasm_store_offset(ctx, offset);
        op
    }

    pub(crate) fn kind(&self, ctx: &Context) -> MemKind {
        *self
            .get_attr_wasm_store_kind(ctx)
            .expect("wasm.store gets its kind from StoreOp::new or its parsed format")
    }

    pub(crate) fn offset(&self, ctx: &Context) -> u32 {
        offset_of(
            &self
                .get_attr_wasm_store_offset(ctx)
                .expect("wasm.store gets its offset from StoreOp::new or its parsed format"),
        )
    }
}

impl Verify for StoreOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        if val_type(ctx, op.get_operand(0).get_type(ctx)) != Some(ValType::I32)
            || val_type(ctx, op.get_operand(1).get_type(ctx)) != Some(self.kind(ctx).value())
        {
            verify_err!(self.loc(ctx), OpErr::Signature("wasm.store".into()))?
        }
        Ok(())
    }
}

/// `memory.size` of memory 0, in 64 KiB pages. Built only by the text parser.
#[pliron_op(
    name = "wasm.memory_size",
    format = "` : ` type($0)",
    interfaces = [OneResultInterface, NOpdsInterface<0>],
    verifier = "succ"
)]
pub(crate) struct MemorySizeOp;

/// `memory.grow` of memory 0 by a number of pages; the previous size, or -1 on failure.
/// Built only by the text parser.
#[pliron_op(
    name = "wasm.memory_grow",
    format = "$0 ` : ` type($0)",
    interfaces = [OneResultInterface, NOpdsInterface<1>],
    verifier = "succ"
)]
pub(crate) struct MemoryGrowOp;

/// A function imported from the host as `module`.`name`; callable with [CallOp].
#[pliron_op(
    name = "wasm.import_func",
    interfaces = [SymbolOpInterface, NOpdsInterface<0>, NResultsInterface<0>, NRegionsInterface<0>],
    attributes = (wasm_import_type: TypeAttr, wasm_import_module: StringAttr, wasm_import_name: StringAttr)
)]
pub struct ImportFuncOp;

impl ImportFuncOp {
    pub fn new(
        ctx: &mut Context,
        symbol: Identifier,
        ty: TypedHandle<FunctionType>,
        module: &str,
        name: &str,
    ) -> Self {
        let op = ImportFuncOp {
            op: Operation::new(ctx, Self::get_concrete_op_info(), vec![], vec![], vec![], 0),
        };
        op.set_symbol_name(ctx, symbol);
        op.set_attr_wasm_import_type(ctx, TypeAttr::new(ty.into()));
        op.set_attr_wasm_import_module(ctx, StringAttr::new(module.to_string()));
        op.set_attr_wasm_import_name(ctx, StringAttr::new(name.to_string()));
        op
    }

    pub(crate) fn module(&self, ctx: &Context) -> String {
        self.get_attr_wasm_import_module(ctx)
            .expect(IMPORT_ATTRS_SET)
            .clone()
            .into()
    }

    pub(crate) fn name(&self, ctx: &Context) -> String {
        self.get_attr_wasm_import_name(ctx)
            .expect(IMPORT_ATTRS_SET)
            .clone()
            .into()
    }
}

/// Why [ImportFuncOp]'s attributes are present: both its constructor and its parser set all three.
const IMPORT_ATTRS_SET: &str = "ImportFuncOp::new and ImportFuncOp::parse set every attribute";

impl Typed for ImportFuncOp {
    fn get_type(&self, ctx: &Context) -> TypeHandle {
        self.get_attr_wasm_import_type(ctx)
            .expect(IMPORT_ATTRS_SET)
            .get_type(ctx)
    }
}

impl Verify for ImportFuncOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if TypedHandle::<FunctionType>::from_handle(self.get_type(ctx), ctx).is_err() {
            verify_err!(self.loc(ctx), OpErr::ValueType("wasm.import_func".into()))?
        }
        Ok(())
    }
}

impl Printable for ImportFuncOp {
    fn fmt(
        &self,
        ctx: &Context,
        state: &pliron::printable::State,
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        typed_symb_op_header(self).fmt(ctx, state, f)?;
        write!(f, " from {:?} {:?}", self.module(ctx), self.name(ctx))
    }
}

impl Parsable for ImportFuncOp {
    type Arg = Vec<(Identifier, pliron::location::Location)>;
    type Parsed = OpObj;

    fn parse<'a>(
        state_stream: &mut StateStream<'a>,
        results: Self::Arg,
    ) -> ParseResult<'a, Self::Parsed> {
        if !results.is_empty() {
            input_err!(
                state_stream.loc(),
                pliron::builtin::op_interfaces::NResultsVerifyErr(0, results.len())
            )?
        }
        let mut parser = (
            spaced(token('@').with(Identifier::parser(()))).skip(spaced(token(':'))),
            spaced(type_parser()),
            spaced(string("from")),
            spaced(StringAttr::parser(())),
            spaced(StringAttr::parser(())),
        );
        parser
            .parse_stream(state_stream)
            .map(|(symbol, ty, _, module, name)| -> OpObj {
                let ctx = &mut state_stream.state.ctx;
                let op = ImportFuncOp {
                    op: Operation::new(
                        ctx,
                        Self::get_concrete_op_info(),
                        vec![],
                        vec![],
                        vec![],
                        0,
                    ),
                };
                op.set_symbol_name(ctx, symbol);
                op.set_attr_wasm_import_type(ctx, TypeAttr::new(ty));
                op.set_attr_wasm_import_module(ctx, module);
                op.set_attr_wasm_import_name(ctx, name);
                OpObj::new(op)
            })
            .into()
    }
}

/// The signature of a callable symbol (a `builtin.func` or a [ImportFuncOp]) visible from `op`.
fn callee_type(
    ctx: &Context,
    op: Ptr<Operation>,
    callee: &Identifier,
) -> Option<TypedHandle<FunctionType>> {
    let mut parent = op.deref(ctx).get_parent_op(ctx);
    while let Some(candidate) = parent {
        let candidate_op = Operation::get_op_dyn(candidate, ctx);
        if let Some(table) = op_cast::<dyn SymbolTableInterface>(candidate_op.as_ref())
            && let Some(symbol) = table.lookup(ctx, callee)
        {
            let ty = match Operation::get_op::<FuncOp>(symbol, ctx) {
                Some(function) => function.get_type(ctx),
                None => Operation::get_op::<ImportFuncOp>(symbol, ctx)?.get_type(ctx),
            };
            return TypedHandle::<FunctionType>::from_handle(ty, ctx).ok();
        }
        parent = candidate.deref(ctx).get_parent_op(ctx);
    }
    None
}

/// A direct call of a function or import of the enclosing module.
#[pliron_op(
    name = "wasm.call",
    format = "attr($wasm_callee, $IdentifierAttr) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))",
    attributes = (wasm_callee: IdentifierAttr)
)]
pub struct CallOp;

impl CallOp {
    pub fn new(
        ctx: &mut Context,
        callee: Identifier,
        args: Vec<Value>,
        results: Vec<TypeHandle>,
    ) -> Self {
        let op = CallOp {
            op: Operation::new(ctx, Self::get_concrete_op_info(), results, args, vec![], 0),
        };
        op.set_attr_wasm_callee(ctx, IdentifierAttr::new(callee));
        op
    }

    pub(crate) fn callee(&self, ctx: &Context) -> Identifier {
        self.get_attr_wasm_callee(ctx)
            .expect("wasm.call gets its callee from CallOp::new or its parsed format")
            .clone()
            .into()
    }
}

impl Verify for CallOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let callee = self.callee(ctx);
        let Some(ty) = callee_type(ctx, self.get_operation(), &callee) else {
            return verify_err!(self.loc(ctx), OpErr::UnknownCallee(callee.to_string()));
        };
        let ty = ty.deref(ctx);
        let op = self.get_operation().deref(ctx);
        let args: Vec<_> = op.operands().map(|v| v.get_type(ctx)).collect();
        let results: Vec<_> = op.results().map(|v| v.get_type(ctx)).collect();
        if args != ty.arg_types() || results != ty.res_types() {
            verify_err!(self.loc(ctx), OpErr::Signature(format!("call to {callee}")))?
        }
        Ok(())
    }
}

/// The function attribute naming its export.
fn export_key() -> Identifier {
    "wasm_export"
        .try_into()
        .expect("wasm_export is a valid identifier")
}

/// Export `function` from the emitted module under `name`.
pub fn set_export(ctx: &mut Context, function: &FuncOp, name: &str) {
    function
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(export_key(), StringAttr::new(name.to_string()));
}

/// The export name of `function`, if it is exported.
pub(crate) fn export_name(ctx: &Context, function: &FuncOp) -> Option<String> {
    function
        .get_operation()
        .deref(ctx)
        .attributes
        .get::<StringAttr>(&export_key())
        .map(|name| name.clone().into())
}
