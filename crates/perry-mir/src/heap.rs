//! Record allocation and field access (`perry.heap` part of the dialect).
//!
//! Allocations carry a [Tier]; field reads and writes are typed against the record's shape.
//! Escape analysis, SROA and tier classification (P3.3, P3.4, P3.7) rewrite these ops.
use crate::{attrs::Tier, types::RecordType};
use pliron::{
    builtin::{
        attributes::IdentifierAttr,
        op_interfaces::{NOpdsInterface, NResultsInterface, OneOpdInterface, OneResultInterface},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    identifier::Identifier,
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
    verify_err,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HeapErr {
    #[error("{0} expects a perry.record, found another type")]
    NotRecord(&'static str),
    #[error("record {record} has {expected} fields but perry.alloc supplies {found}")]
    FieldCount {
        record: String,
        expected: usize,
        found: usize,
    },
    #[error("record {record} field {field} has a different type than its value")]
    FieldType { record: String, field: String },
    #[error("record {record} has no field {field}")]
    NoField { record: String, field: String },
    #[error("perry.alloc tier {0:?} is not an allocation tier")]
    Tier(Tier),
}

fn record<'a>(
    ctx: &'a Context,
    ty: TypeHandle,
    op: &'static str,
) -> Result<std::cell::Ref<'a, RecordType>> {
    match TypedHandle::<RecordType>::from_handle(ty, ctx) {
        Ok(record) => Ok(record.deref(ctx)),
        Err(_) => verify_err!(pliron::location::Location::Unknown, HeapErr::NotRecord(op)),
    }
}

/// Allocate a record whose fields are initialized from the operands in declaration order.
#[pliron_op(
    name = "perry.alloc",
    format = "attr($perry_tier, $Tier) ` (` operands(CharSpace(`,`)) `) : ` type($0)",
    interfaces = [OneResultInterface],
    attributes = (perry_tier: Tier)
)]
pub struct AllocOp;

impl AllocOp {
    pub fn new(ctx: &mut Context, record: TypedHandle<RecordType>, fields: Vec<Value>) -> Self {
        let op = AllocOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![record.into()],
                fields,
                vec![],
                0,
            ),
        };
        op.set_attr_perry_tier(ctx, Tier::Unassigned);
        op
    }

    pub fn tier(&self, ctx: &Context) -> Tier {
        *self.get_attr_perry_tier(ctx).expect("alloc tier")
    }

    pub fn set_tier(&self, ctx: &Context, tier: Tier) {
        self.set_attr_perry_tier(ctx, tier);
    }
}

impl Verify for AllocOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let loc = self.loc(ctx);
        // A scalar-tier record has been replaced by its fields; the op must be gone.
        if self.tier(ctx) == Tier::Scalar {
            verify_err!(loc.clone(), HeapErr::Tier(Tier::Scalar))?
        }
        let record = record(ctx, self.get_result(ctx).get_type(ctx), "perry.alloc")?;
        let fields = record.fields()?;
        let operands: Vec<Value> = self.get_operation().deref(ctx).operands().collect();
        if fields.len() != operands.len() {
            verify_err!(
                loc.clone(),
                HeapErr::FieldCount {
                    record: record.name().to_string(),
                    expected: fields.len(),
                    found: operands.len(),
                }
            )?
        }
        for ((field, ty), value) in fields.iter().zip(operands) {
            if value.get_type(ctx) != *ty {
                verify_err!(
                    loc.clone(),
                    HeapErr::FieldType {
                        record: record.name().to_string(),
                        field: field.to_string(),
                    }
                )?
            }
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for AllocOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// The record type of `reference` and the type of its `field`, or a verification error.
fn field_type(
    ctx: &Context,
    reference: Value,
    field: &Identifier,
    op: &'static str,
) -> Result<TypeHandle> {
    let record = record(ctx, reference.get_type(ctx), op)?;
    match record.field(field) {
        Some((_, ty)) => Ok(ty),
        None => verify_err!(
            pliron::location::Location::Unknown,
            HeapErr::NoField {
                record: record.name().to_string(),
                field: field.to_string(),
            }
        ),
    }
}

/// Read one field of a record.
#[pliron_op(
    name = "perry.field_get",
    format = "$0 `.` attr($perry_get_field, $IdentifierAttr) ` : ` type($0)",
    interfaces = [OneOpdInterface, OneResultInterface],
    attributes = (perry_get_field: IdentifierAttr)
)]
pub struct FieldGetOp;

impl FieldGetOp {
    pub fn new(ctx: &mut Context, reference: Value, field: Identifier) -> Result<Self> {
        let ty = field_type(ctx, reference, &field, "perry.field_get")?;
        let op = FieldGetOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![reference],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_get_field(ctx, IdentifierAttr::new(field));
        Ok(op)
    }

    pub fn field(&self, ctx: &Context) -> Identifier {
        self.get_attr_perry_get_field(ctx)
            .expect("field name")
            .clone()
            .into()
    }
}

impl Verify for FieldGetOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let reference = self.get_operation().deref(ctx).get_operand(0);
        let ty = field_type(ctx, reference, &self.field(ctx), "perry.field_get")?;
        if ty != self.get_result(ctx).get_type(ctx) {
            let record = record(ctx, reference.get_type(ctx), "perry.field_get")?;
            verify_err!(
                self.loc(ctx),
                HeapErr::FieldType {
                    record: record.name().to_string(),
                    field: self.field(ctx).to_string(),
                }
            )?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for FieldGetOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

/// Write one field of a record.
#[pliron_op(
    name = "perry.field_set",
    format = "$0 `.` attr($perry_set_field, $IdentifierAttr) ` = ` $1",
    interfaces = [NOpdsInterface<2>, NResultsInterface<0>],
    attributes = (perry_set_field: IdentifierAttr)
)]
pub struct FieldSetOp;

impl FieldSetOp {
    pub fn new(ctx: &mut Context, reference: Value, field: Identifier, value: Value) -> Self {
        let op = FieldSetOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![],
                vec![reference, value],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_set_field(ctx, IdentifierAttr::new(field));
        op
    }

    pub fn field(&self, ctx: &Context) -> Identifier {
        self.get_attr_perry_set_field(ctx)
            .expect("field name")
            .clone()
            .into()
    }
}

impl Verify for FieldSetOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        let (reference, value) = (op.get_operand(0), op.get_operand(1));
        let ty = field_type(ctx, reference, &self.field(ctx), "perry.field_set")?;
        if ty != value.get_type(ctx) {
            let record = record(ctx, reference.get_type(ctx), "perry.field_set")?;
            verify_err!(
                self.loc(ctx),
                HeapErr::FieldType {
                    record: record.name().to_string(),
                    field: self.field(ctx).to_string(),
                }
            )?
        }
        Ok(())
    }
}
