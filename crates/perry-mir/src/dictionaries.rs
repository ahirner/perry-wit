//! Typed ordered dictionaries, distinct from closed records and JSON variant trees.
use crate::{
    attrs::{ErrorKind, Tier},
    exceptions::MayThrowInterface,
    types::{ArrayType, DictionaryType, OptionalType, StringType},
};
use pliron::{
    builtin::{
        op_interfaces::OneResultInterface,
        types::{IntegerType, Signedness},
    },
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_attr, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};

#[derive(Debug, thiserror::Error)]
#[error("dictionary operation has incorrect operand or result types")]
struct SignatureError;

pub fn element(ctx: &Context, ty: TypeHandle) -> Option<TypeHandle> {
    ty.deref(ctx)
        .downcast_ref::<DictionaryType>()
        .and_then(DictionaryType::element)
}

#[pliron_op(
    name = "perry.dictionary_new",
    format = "attr($perry_dictionary_tier, $Tier) ` : ` type($0)",
    interfaces = [OneResultInterface],
    attributes = (perry_dictionary_tier: Tier)
)]
pub struct NewOp;
impl NewOp {
    pub fn new(ctx: &mut Context, element: TypeHandle) -> Self {
        let ty = DictionaryType::get(ctx, element).into();
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_dictionary_tier(ctx, Tier::Unassigned);
        op
    }
}
impl Verify for NewOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if element(ctx, self.get_result(ctx).get_type(ctx)).is_none()
            || self.get_operation().deref(ctx).get_num_operands() != 0
        {
            verify_err!(self.loc(ctx), SignatureError)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for NewOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}

#[pliron_attr(name = "perry.dictionary_action", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum Action {
    Get,
    Set,
    Delete,
    Has,
    Keys,
    Values,
}
impl Action {
    fn result(self, ctx: &mut Context, entry: TypeHandle) -> Vec<TypeHandle> {
        match self {
            Self::Get => vec![if entry.deref(ctx).is::<OptionalType>() {
                entry
            } else {
                OptionalType::get(ctx, entry).into()
            }],
            Self::Set => vec![],
            Self::Delete | Self::Has => vec![IntegerType::get(ctx, 1, Signedness::Signless).into()],
            Self::Keys => {
                let string = StringType::get(ctx).into();
                vec![ArrayType::get(ctx, string).into()]
            }
            Self::Values => vec![ArrayType::get(ctx, entry).into()],
        }
    }
}

#[pliron_op(
    name = "perry.dictionary",
    format = "attr($perry_dictionary_action, $Action) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))",
    attributes = (perry_dictionary_action: Action)
)]
pub struct AccessOp;
impl AccessOp {
    pub fn new(ctx: &mut Context, action: Action, args: Vec<Value>) -> Self {
        let entry = element(ctx, args[0].get_type(ctx)).expect("dictionary operand");
        let results = action.result(ctx, entry);
        let op = Self {
            op: Operation::new(ctx, Self::get_concrete_op_info(), results, args, vec![], 0),
        };
        op.set_attr_perry_dictionary_action(ctx, action);
        op
    }
    pub fn action(&self, ctx: &Context) -> Action {
        *self
            .get_attr_perry_dictionary_action(ctx)
            .expect("dictionary action")
    }
}
impl Verify for AccessOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let action = self.action(ctx);
        let op = self.get_operation().deref(ctx);
        let count = match action {
            Action::Set => 3,
            Action::Keys | Action::Values => 1,
            _ => 2,
        };
        if op.get_num_operands() != count {
            verify_err!(self.loc(ctx), SignatureError)?;
        }
        let Some(entry) = element(ctx, op.get_operand(0).get_type(ctx)) else {
            return verify_err!(self.loc(ctx), SignatureError);
        };
        if count > 1
            && !op
                .get_operand(1)
                .get_type(ctx)
                .deref(ctx)
                .is::<StringType>()
            || action == Action::Set && op.get_operand(2).get_type(ctx) != entry
        {
            verify_err!(self.loc(ctx), SignatureError)?;
        }
        let result_count = usize::from(action != Action::Set);
        if op.get_num_results() != result_count {
            verify_err!(self.loc(ctx), SignatureError)?;
        }
        if result_count == 0 {
            return Ok(());
        }
        let result = op.get_result(0).get_type(ctx);
        let valid = match action {
            Action::Get => {
                if entry.deref(ctx).is::<OptionalType>() {
                    result == entry
                } else {
                    result
                        .deref(ctx)
                        .downcast_ref::<OptionalType>()
                        .is_some_and(|ty| ty.value() == entry)
                }
            }
            Action::Delete | Action::Has => result
                .deref(ctx)
                .downcast_ref::<IntegerType>()
                .is_some_and(|ty| ty.width() == 1),
            Action::Keys => result
                .deref(ctx)
                .downcast_ref::<ArrayType>()
                .is_some_and(|ty| ty.element().deref(ctx).is::<StringType>()),
            Action::Values => result
                .deref(ctx)
                .downcast_ref::<ArrayType>()
                .is_some_and(|ty| ty.element() == entry),
            Action::Set => unreachable!(),
        };
        if !valid {
            verify_err!(self.loc(ctx), SignatureError)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl MayThrowInterface for AccessOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}
