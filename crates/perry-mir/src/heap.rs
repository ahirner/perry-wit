//! Record allocation and field access (`perry.heap` part of the dialect).
//!
//! Allocations carry a [Tier], and field reads and writes are typed against the record's shape.
//! Escape analysis, SROA, and tier classification refine and rewrite these ops.
use crate::operation_schema::op_schema;
use crate::signatures::{MissingProjectionField, Ty, TypeMismatch, operand_types, type_mismatch};
use crate::{
    attrs::{FieldOrder, Tier},
    types::{ArrayType, RecordType},
};
use pliron::{
    builtin::{attributes::StringAttr, op_interfaces::OneResultInterface},
    common_traits::Verify,
    context::Context,
    op::Op,
    operation::Operation,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
    verify_err,
};

#[derive(Debug)]
pub(crate) enum HeapErr {
    NotRecord(&'static str),
    NotArray,
    NotObject,
    FieldCount {
        record: String,
        expected: usize,
        found: usize,
    },
    FieldType {
        record: String,
        field: String,
    },
    NoField {
        record: String,
        field: String,
    },
    Tier(Tier),
    MissingTier,
    FieldOrder(String),
}

impl core::fmt::Display for HeapErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotRecord(op) => write!(f, "{op} expects a perry.record, found another type"),
            Self::NotArray => f.write_str("record enumeration projection must be an array"),
            Self::NotObject => f.write_str("record enumeration order requires a non-tuple record"),
            Self::FieldCount {
                record,
                expected,
                found,
            } => write!(
                f,
                "record {record} has {expected} fields but perry.alloc supplies {found}"
            ),
            Self::FieldType { record, field } => {
                write!(
                    f,
                    "record {record} field {field} has a different type than its value"
                )
            }
            Self::NoField { record, field } => write!(f, "record {record} has no field {field}"),
            Self::Tier(tier) => write!(f, "perry.alloc tier {tier:?} is not an allocation tier"),
            Self::MissingTier => f.write_str("perry.alloc is missing its allocation tier"),
            Self::FieldOrder(record) => {
                write!(f, "record {record} has an invalid property insertion order")
            }
        }
    }
}

impl core::error::Error for HeapErr {}

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

op_schema! {
    /// Allocate a record whose fields are initialized from the operands in declaration order.
    pub AllocOp(..operands) -> fallible;
    name = "perry.alloc";
    format = "attr($perry_tier, $Tier) ` (` operands(CharSpace(`,`)) `) ` opt_attr($perry_field_order, $FieldOrder, label($order)) ` : ` type($0)";
    attributes = (perry_tier: Tier, perry_field_order: FieldOrder);
    constructors {
        pub new(record: TypedHandle<RecordType>, fields: Vec<Value>) operands(fields)
            -> infallible => explicit(record.into());
    }
    attribute(tier: Tier, get_attr_perry_tier, set_attr_perry_tier) =
        Tier::Unassigned => HeapErr::MissingTier;
    prepare(this) {
        // A scalar-tier record has been replaced by its fields; the op must be gone.
        if *tier == Tier::Scalar {
            return verify_err!(this.loc(ctx), HeapErr::Tier(Tier::Scalar));
        }
        <Self as OneResultInterface>::verify(this, ctx)?;
        let ty = this.get_result(ctx).get_type(ctx);
        let order = this.get_attr_perry_field_order(ctx);
        let order = order.as_deref();
    }
    result(ctx, ty: TypeHandle, operands: &Operation, order: Option<&FieldOrder>): TypeHandle = {
        let record = record(ctx, ty, "perry.alloc")?;
        let fields = record.fields()?;
        if order.is_some_and(|order| {
            order.0.iter().enumerate().any(|(position, &field)| {
                field as usize >= fields.len() || order.0[..position].contains(&field)
            })
        }) {
            return verify_err!(
                operands.loc().clone(), HeapErr::FieldOrder(record.name().to_string())
            );
        }
        if let Some(mismatch) = type_mismatch(
            ctx,
            fields.iter().map(|(_, ty)| *ty),
            operand_types(ctx, operands),
        ) {
            let record = record.name().to_string();
            let error = match mismatch {
                TypeMismatch::Count => HeapErr::FieldCount {
                    record,
                    expected: fields.len(),
                    found: operands.get_num_operands(),
                },
                TypeMismatch::Type(index) => HeapErr::FieldType {
                    record, field: fields[index].0.clone(),
                },
            };
            return verify_err!(operands.loc().clone(), error);
        }
        Ok(ty)
    };
    error = HeapErr::NotRecord("perry.alloc");
}

impl AllocOp {
    pub(crate) fn set_tier(&self, ctx: &Context, tier: Tier) {
        self.set_attr_perry_tier(ctx, tier);
    }
}

/// The record type of `reference` and the type of its `field`, or a verification error.
fn field_type(
    ctx: &Context,
    reference: Value,
    field: &str,
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

op_schema! {
    /// Read one field of a record.
    pub FieldGetOp(reference) -> fallible;
    name = "perry.field_get";
    format = "$0 `.` attr($perry_get_field, $StringAttr) ` : ` type($0)";
    attributes = (perry_get_field: StringAttr);
    constructors { pub new(reference: Value, field: String) => infer(reference, field.as_str()); }
    attribute(field: String, get_attr_perry_get_field, set_attr_perry_get_field) =
        StringAttr::new(field) => MissingProjectionField;
    prepare(_this) { let field = field.as_str(); }
    result(ctx, reference: Value, field: &str): TypeHandle = {
        field_type(ctx, reference, field, "perry.field_get")
    };
    error = HeapErr::FieldType {
        record: record(ctx, reference.get_type(ctx), "perry.field_get")?.name().to_string(),
        field: field.to_owned(),
    };
}

op_schema! {
    /// Test whether one field of a record is present (not omitted).
    pub FieldPresentOp(reference) -> fallible;
    name = "perry.field_present";
    format = "$0 `.` attr($perry_present_field, $StringAttr) ` : ` type($0)";
    attributes = (perry_present_field: StringAttr);
    constructors { pub new(reference: Value, field: String) => infer(reference, field.as_str()); }
    attribute(field: String, get_attr_perry_present_field, set_attr_perry_present_field) =
        StringAttr::new(field) => MissingProjectionField;
    prepare(_this) { let field = field.as_str(); }
    result(ctx, reference: Value, field: &str): Ty = {
        field_type(ctx, reference, field, "perry.field_present")?;
        Ok(Ty::Predicate)
    };
    error = HeapErr::FieldType {
        record: record(ctx, reference.get_type(ctx), "perry.field_present")?.name().to_string(),
        field: field.to_owned(),
    };
}

op_schema! {
    /// Write one field of a record.
    pub FieldSetOp(reference, value) -> infallible;
    name = "perry.field_set";
    format = "$0 `.` attr($perry_set_field, $StringAttr) ` = ` $1";
    attributes = (perry_set_field: StringAttr);
    constructors { pub new(reference: Value, field: String, value: Value) => empty(); }
    attribute(field: String, get_attr_perry_set_field, set_attr_perry_set_field) =
        StringAttr::new(field) => MissingProjectionField;
    prepare(_this) { let field = field.as_str(); }
    result(ctx): () = ();
    error = HeapErr::NotRecord("perry.field_set");
    check field_type(ctx, reference, field, "perry.field_set")? == value.get_type(ctx)
        => HeapErr::FieldType {
            record: record(ctx, reference.get_type(ctx), "perry.field_set")?.name().to_string(),
            field: field.to_owned(),
        };
}

op_schema! {
    /// Filter and order a canonical record-field projection by the instance's property metadata.
    pub RecordOrderOp(reference, projection) -> infallible;
    name = "perry.record_order";
    format = "$0 `, ` $1";
    constructors { pub new(reference: Value, projection: Value) => empty(); }
    result(ctx): () = ();
    error = HeapErr::NotRecord("perry.record_order");
    check reference.get_type(ctx).deref(ctx).downcast_ref::<RecordType>()
        .is_some_and(|record| !record.is_tuple()) => HeapErr::NotObject;
    check projection.get_type(ctx).deref(ctx).is::<ArrayType>() => HeapErr::NotArray;
}
