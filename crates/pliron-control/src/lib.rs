//! Shared branch definitions for pliron dialects with distinct condition representations.
//!
//! Adapted from `pliron-llvm` (Apache-2.0, The pliron contributors).

/// Define `BrOp`, `CondBrOp`, and `ReturnOp` in a module importing pliron and thiserror.
/// The predicate checks the dialect's condition type; operation names and errors stay local.
#[macro_export]
macro_rules! control_flow_ops {
    ($branch:literal, $conditional:literal, $return:literal, $valid:expr, $description:literal) => {
        use core::ops::Range;
        use pliron::{
            basic_block::BasicBlock,
            builtin::op_interfaces::{
                BranchOpInterface, IsTerminatorInterface, NResultsInterface, NSuccsInterface,
                OneSuccInterface, OperandSegmentInterface,
            },
            combine::Parser,
            common_traits::{Named, Verify},
            context::{Context, Ptr},
            derive::{op_interface_impl, pliron_op},
            identifier::Identifier,
            input_err,
            irfmt::{
                parsers::{
                    block_opd_parser, delimited_list_parser, spaced,
                    ssa_opd_parser,
                },
                printers::iter_with_sep,
            },
            location::Location,
            op::{Op, OpObj},
            operation::Operation,
            parsable::{Parsable, ParseResult, StateStream, StreamLocation},
            printable::{ListSeparator, Printable},
            result::Result,
            value::Value,
            verify_err,
        };
        use thiserror::Error;

        /// Unconditional branch passing its operands as the destination's block arguments.
        #[pliron_op(
            name = $branch,
            format = "succ($0) `(` operands(CharSpace(`,`)) `)`",
            interfaces = [IsTerminatorInterface, NResultsInterface<0>, OneSuccInterface],
            verifier = "succ"
        )]
        pub struct BrOp;

        impl BrOp {
            pub fn new(ctx: &mut Context, dest: Ptr<BasicBlock>, args: Vec<Value>) -> Self {
                Operation::new_op::<Self>(ctx, vec![], args, vec![dest], 0)
            }
        }

        #[op_interface_impl]
        impl BranchOpInterface for BrOp {
            fn verify_successor_operand_layout(&self, ctx: &Context) -> Result<()> {
                <Self as OneSuccInterface>::verify(self, ctx)
            }

            fn successor_operand_range(&self, ctx: &Context, succ_idx: usize) -> Range<usize> {
                assert_eq!(succ_idx, 0, concat!($branch, " has one successor"));
                0..self.get_operation().deref(ctx).get_num_operands()
            }

            fn add_successor_operand(&self, ctx: &mut Context, succ_idx: usize, operand: Value) -> usize {
                assert_eq!(succ_idx, 0, concat!($branch, " has one successor"));
                Operation::push_operand(self.get_operation(), ctx, operand)
            }

            fn remove_successor_operand(
                &self,
                ctx: &mut Context,
                succ_idx: usize,
                arg_idx: usize,
            ) -> Value {
                assert_eq!(succ_idx, 0, concat!($branch, " has one successor"));
                Operation::remove_operand(self.get_operation(), ctx, arg_idx)
            }
        }

        /// Conditional branch with separate block arguments for each destination.
        #[pliron_op(
            name = $conditional,
            interfaces = [IsTerminatorInterface, NResultsInterface<0>, NSuccsInterface<2>],
            operands = (condition)
        )]
        pub struct CondBrOp;

        impl CondBrOp {
            pub fn new(
                ctx: &mut Context,
                condition: Value,
                then_dest: Ptr<BasicBlock>,
                then_args: Vec<Value>,
                else_dest: Ptr<BasicBlock>,
                else_args: Vec<Value>,
            ) -> Self {
                let (operands, sizes) =
                    Self::compute_segment_sizes(vec![vec![condition], then_args, else_args]);
                let op = Operation::new_op::<Self>(ctx, vec![], operands, vec![then_dest, else_dest], 0);
                op.set_operand_segment_sizes(ctx, sizes);
                op
            }
        }

        #[derive(Debug, Error)]
        enum CondBrErr {
            #[error("{name} condition must be {expected}", name = $conditional, expected = $description)]
            Condition,
            #[error("{name} needs exactly one condition operand, found {0}", name = $conditional)]
            ConditionCount(u32),
        }

        impl Verify for CondBrOp {
            fn verify(&self, ctx: &Context) -> Result<()> {
                let count = self.segment_size(ctx, 0);
                if count != 1 {
                    verify_err!(self.loc(ctx), CondBrErr::ConditionCount(count))?
                }
                if !($valid)(ctx, self.get_operand_condition(ctx)) {
                    verify_err!(self.loc(ctx), CondBrErr::Condition)?
                }
                Ok(())
            }
        }

        #[op_interface_impl]
        impl OperandSegmentInterface for CondBrOp {
            fn expected_num_segments(&self, _ctx: &Context) -> Option<usize> {
                Some(3)
            }
        }

        #[op_interface_impl]
        impl BranchOpInterface for CondBrOp {
            fn verify_successor_operand_layout(&self, ctx: &Context) -> Result<()> {
                <Self as OperandSegmentInterface>::verify(self, ctx)?;
                <Self as NSuccsInterface<2>>::verify(self, ctx)
            }

            fn successor_operand_range(&self, ctx: &Context, succ_idx: usize) -> Range<usize> {
                assert!(succ_idx < 2, concat!($conditional, " has two successors"));
                self.segment_range(ctx, succ_idx + 1)
            }

            fn add_successor_operand(&self, ctx: &mut Context, succ_idx: usize, operand: Value) -> usize {
                self.push_to_segment(ctx, succ_idx + 1, operand)
            }

            fn remove_successor_operand(
                &self,
                ctx: &mut Context,
                succ_idx: usize,
                arg_idx: usize,
            ) -> Value {
                self.remove_from_segment(ctx, succ_idx + 1, arg_idx)
            }
        }

        impl Printable for CondBrOp {
            fn fmt(
                &self,
                ctx: &Context,
                state: &pliron::printable::State,
                f: &mut core::fmt::Formatter<'_>,
            ) -> core::fmt::Result {
                let op = self.get_operation().deref(ctx);
                let args = |index| {
                    iter_with_sep(
                        self.successor_operands(ctx, index).into_iter(),
                        ListSeparator::CharSpace(','),
                    )
                };
                write!(
                    f,
                    "{} if {} ^{}({}) else ^{}({})",
                    Self::get_opid_static(),
                    op.get_operand(0).print(ctx, state),
                    op.get_successor(0).deref(ctx).unique_name(ctx),
                    args(0).print(ctx, state),
                    op.get_successor(1).deref(ctx).unique_name(ctx),
                    args(1).print(ctx, state),
                )
            }
        }

        impl Parsable for CondBrOp {
            type Arg = Vec<(Identifier, Location)>;
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
                let keyword = |word: &'static str| spaced(pliron::combine::parser::char::string(word));
                let args = || delimited_list_parser('(', ')', ',', ssa_opd_parser());
                let ((((condition, then_dest), then_args), (else_dest, else_args)), committed) = keyword("if")
                    .with(spaced(ssa_opd_parser()))
                    .and(spaced(block_opd_parser()))
                    .and(args())
                    .and(keyword("else").with(spaced(block_opd_parser()).and(args())))
                    .parse_stream(state_stream)
                    .into_result()?;
                let op = CondBrOp::new(state_stream.state.ctx, condition, then_dest, then_args, else_dest, else_args);
                Ok((OpObj::new(op), committed))
            }
        }

        /// Function return of zero or more values.
        #[pliron_op(
            name = $return,
            format = "operands(CharSpace(`,`))",
            interfaces = [IsTerminatorInterface, NResultsInterface<0>],
            verifier = "succ"
        )]
        pub struct ReturnOp;

        impl ReturnOp {
            pub fn new(ctx: &mut Context, values: Vec<Value>) -> Self {
                Operation::new_op::<Self>(ctx, vec![], values, vec![], 0)
            }
        }
    };
}
