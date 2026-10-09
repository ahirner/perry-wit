//! Perry HIR → perry-mir builder (`TODOs.md` P2.2).
//!
//! One structural lowering with no representation decisions:
//! - every local (parameters included) is a `perry.local` slot, and pliron `mem2reg` builds SSA
//!   joins, loop-carried block arguments and later await continuations;
//! - expression types come from the MIR types of the lowered operands, so there is no separate,
//!   recursive type inference;
//! - tiers stay `Unassigned` and layouts abstract; later passes decide them.
//!
//! Supported so far: synchronous functions over `number`, `boolean`, `string`, records
//! (interfaces, type literals and object literals of the same shape) and dense arrays; locals,
//! assignment and `++`/`--`; arithmetic, comparisons, `&&`/`||`, `!`, unary `-`/`+`, `?:`;
//! `if`, `while`, `do`/`while`, `for`, unlabeled `break`/`continue`, `return`; direct calls;
//! string concatenation; field reads and writes; array literals, `push`, indexing and
//! `.length`. Everything else is rejected with a diagnostic.
mod exceptions;
mod shapes;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use exceptions::{Cleanup, Handler};
use perry_hir::{
    ir::{
        BinaryOp, CompareOp, Expr, Function, LogicalOp, Module as HirModule, Stmt, UnaryOp,
        UpdateOp,
    },
    types::{FuncId, LocalId, Type as HirType},
};
use perry_mir::{
    attrs::{BinaryKind, CompareKind, ConvertKind},
    calls::{AsyncCallOp, AwaitOp, CallOp, TimerOp, set_async},
    collections::{
        ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp, StringConcatOp, StringConstOp,
        StringLenOp,
    },
    control::{BrOp, CondBrOp, ReturnOp},
    exceptions::{
        BoxExceptionOp, CatchOp, CaughtStatusOp, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp,
        MayThrowInterface, RaiseOp, RethrowOp,
    },
    heap::{AllocOp, FieldGetOp, FieldSetOp},
    intrinsics::{Intrinsic, IntrinsicOp},
    locals::{LoadOp, LocalOp, StoreOp, UndefOp},
    optional::{
        IsUndefinedOp, ToNumberOp, ToStringOp, UndefinedOp, UnwrapOp, WrapOp, optional_value,
    },
    scalar::{BinaryOp as MirBinary, CompareOp as MirCompare, ConvertOp},
    types::{ArrayType, ExceptionType, PromiseType, RecordType, StringType},
};
use pliron::printable::Printable as _;
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        attributes::{FPDoubleAttr, IntegerAttr},
        op_interfaces::{OneResultInterface, SingleBlockRegionInterface},
        ops::{ConstantOp, FuncOp, ModuleOp},
        types::{FP64Type, FunctionType, IntegerType, Signedness},
    },
    context::{Context, Ptr},
    identifier::Identifier,
    irbuild::{
        inserter::{IRInserter, Inserter},
        listener::DummyListener,
    },
    linked_list::ContainsLinkedList,
    op::{Op, op_cast},
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::{APInt, bw},
    value::Value,
};
use shapes::{Shapes, identifier};
use std::collections::{HashMap, HashSet};

/// How top-level statements enter the component.
#[derive(Clone)]
pub enum EntryPoint {
    Exports,
    Command { implementation: String },
}

pub const COMMAND_ENTRY: &str = "perry_command_main";

/// Parse TypeScript, lower it to Perry HIR, and build a verified-ready MIR module.
pub fn lower_source(
    ctx: &mut Context,
    source: &str,
    file: &str,
    entry: EntryPoint,
) -> Result<ModuleOp> {
    let ast = perry_parser::parse_typescript(source, file)
        .map_err(|error| anyhow!("failed to parse {file}: {error:?}"))?;
    let mut hir = perry_hir::lower_module(&ast, "main", file)
        .map_err(|error| anyhow!("failed to lower {file}: {error:?}"))?;
    match entry {
        EntryPoint::Command { implementation } => {
            ensure!(
                !hir.functions.iter().any(|f| f.name == COMMAND_ENTRY),
                "{COMMAND_ENTRY} is reserved for the command entry point"
            );
            let id = hir
                .functions
                .iter()
                .map(|f| f.id)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .context("too many functions")?;
            let mut body = std::mem::take(&mut hir.init);
            let result = if let Some(function) = hir
                .functions
                .iter()
                .find(|f| f.is_exported && f.name == implementation)
            {
                ensure!(
                    function.params.is_empty(),
                    "command implementation {implementation} must take no parameters"
                );
                let call = Expr::Call {
                    callee: Box::new(Expr::FuncRef(function.id)),
                    args: vec![],
                    type_args: vec![],
                    byte_offset: 0,
                };
                let value = if function.is_async {
                    Expr::Await(Box::new(call))
                } else {
                    call
                };
                Expr::PropertyGet {
                    object: Box::new(value),
                    property: "ok".into(),
                    byte_offset: 0,
                }
            } else {
                Expr::Bool(true)
            };
            body.push(Stmt::Return(Some(result)));
            hir.functions.push(Function {
                id,
                name: COMMAND_ENTRY.to_string(),
                type_params: vec![],
                params: vec![],
                return_type: HirType::Boolean,
                body,
                is_async: true,
                is_generator: false,
                is_strict: hir.init_is_strict,
                is_exported: true,
                captures: vec![],
                decorators: vec![],
                was_plain_async: false,
                was_unrolled: false,
            });
        }
        EntryPoint::Exports => ensure!(
            hir.init.is_empty(),
            "top-level execution needs a wasi:cli/run command world; module initialization is not supported yet"
        ),
    }
    lower_module(ctx, &hir)
}

struct Signature {
    name: Identifier,
    /// `async`: callers start it with `perry.async_call` and get a promise of `results[0]`.
    is_async: bool,
    params: Vec<TypeHandle>,
    results: Vec<TypeHandle>,
}

/// Build a MIR module from prepared HIR functions. Command preparation moves module
/// statements into the entry function before this step.
pub fn lower_module(ctx: &mut Context, hir: &HirModule) -> Result<ModuleOp> {
    let module = ModuleOp::new(ctx, identifier(&hir_module_name(hir))?);
    let shapes = Shapes::new(hir);
    let timers: HashSet<String> = hir
        .imports
        .iter()
        .filter(|import| {
            matches!(
                import.source.as_str(),
                "timers/promises" | "node:timers/promises"
            )
        })
        .flat_map(|import| import.specifiers.iter())
        .filter_map(|specifier| match specifier {
            perry_hir::ir::ImportSpecifier::Named { imported, local }
                if imported == "setTimeout" =>
            {
                Some(local.clone())
            }
            _ => None,
        })
        .collect();
    let mut signatures = HashMap::new();
    for function in &hir.functions {
        ensure!(
            !function.is_generator,
            "generator function {} is not supported yet",
            function.name
        );
        ensure!(
            function.captures.is_empty(),
            "closures are not supported (subset rule R5): {}",
            function.name
        );
        let params = function
            .params
            .iter()
            .map(|param| {
                ensure!(
                    param.default.is_none() && !param.is_rest,
                    "default and rest parameters are not supported yet: {}",
                    function.name
                );
                shapes
                    .value_type(ctx, &param.ty)
                    .with_context(|| format!("parameter {} of {}", param.name, function.name))
            })
            .collect::<Result<Vec<_>>>()?;
        let results = match (&function.return_type, function.is_async) {
            (HirType::Void, false) => vec![],
            // An async function settles its promise with its result; `void` settles `undefined`.
            (HirType::Promise(inner), true) => vec![shapes.settled_type(ctx, inner)?],
            (ty, true) => vec![shapes.settled_type(ctx, ty)?],
            (ty, false) => {
                vec![
                    shapes
                        .value_type(ctx, ty)
                        .with_context(|| format!("result of {}", function.name))?,
                ]
            }
        };
        signatures.insert(
            function.id,
            Signature {
                is_async: function.is_async,
                name: identifier(&function.name)?,
                params,
                results,
            },
        );
    }
    for function in &hir.functions {
        let func = FunctionBuilder::lower(ctx, function, &signatures, &shapes, &timers)
            .with_context(|| format!("lowering function {}", function.name))?;
        if function.is_exported {
            perry_mir::calls::set_exported(ctx, &func);
        }
        if function.is_async {
            set_async(ctx, &func);
        }
        module.append_operation(ctx, func.get_operation(), 0);
    }
    Ok(module)
}

fn hir_module_name(hir: &HirModule) -> String {
    let name: String = hir
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        format!("m_{name}")
    } else {
        name
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Boolean,
    String,
    Record,
    Array,
    Exception,
}

fn kind(ctx: &Context, value: Value) -> Result<Kind> {
    let ty = value.get_type(ctx);
    let ty = ty.deref(ctx);
    if ty.is::<FP64Type>() {
        Ok(Kind::Number)
    } else if ty.is::<StringType>() {
        Ok(Kind::String)
    } else if ty.is::<ExceptionType>() {
        Ok(Kind::Exception)
    } else if ty.is::<RecordType>() {
        Ok(Kind::Record)
    } else if ty.is::<ArrayType>() {
        Ok(Kind::Array)
    } else if ty
        .downcast_ref::<IntegerType>()
        .is_some_and(|int| int.width() == 1)
    {
        Ok(Kind::Boolean)
    } else {
        bail!("unsupported value type in expression")
    }
}

#[derive(Clone, Copy)]
struct Loop {
    cleanup_depth: usize,
    continue_to: Ptr<BasicBlock>,
    break_to: Ptr<BasicBlock>,
}

struct FunctionBuilder<'s> {
    region: Ptr<pliron::region::Region>,
    ins: IRInserter<DummyListener>,
    block: Ptr<BasicBlock>,
    locals: HashMap<LocalId, Value>,
    loops: Vec<Loop>,
    handlers: Vec<Handler>,
    cleanups: Vec<Cleanup>,
    signatures: &'s HashMap<FuncId, Signature>,
    shapes: &'s Shapes<'s>,
    /// Local names bound to `setTimeout` from `node:timers/promises`.
    timers: &'s HashSet<String>,
    results: Vec<TypeHandle>,
    is_async: bool,
    labels: usize,
}

impl<'s> FunctionBuilder<'s> {
    fn lower(
        ctx: &mut Context,
        function: &Function,
        signatures: &'s HashMap<FuncId, Signature>,
        shapes: &'s Shapes<'s>,
        timers: &'s HashSet<String>,
    ) -> Result<FuncOp> {
        let signature = &signatures[&function.id];
        let ty = FunctionType::get(ctx, signature.params.clone(), signature.results.clone());
        let func = FuncOp::new(ctx, signature.name.clone(), ty);
        let entry = func.get_entry_block(ctx);
        let region = func.get_operation().deref(ctx).get_region(0);
        let mut builder = FunctionBuilder {
            region,
            ins: IRInserter::new_at_block_end(entry),
            block: entry,
            locals: HashMap::new(),
            loops: Vec::new(),
            handlers: Vec::new(),
            cleanups: Vec::new(),
            signatures,
            shapes,
            timers,
            results: signature.results.clone(),
            is_async: signature.is_async,
            labels: 0,
        };
        for (index, param) in function.params.iter().enumerate() {
            let value = entry.deref(ctx).get_argument(index);
            let slot = builder.slot(ctx, value.get_type(ctx));
            builder.store(ctx, slot, value);
            builder.locals.insert(param.id, slot);
        }
        builder.statements(ctx, &function.body)?;
        if !builder.terminated(ctx) {
            let values = builder.implicit_results(ctx);
            builder.emit(ctx, |ctx| ReturnOp::new(ctx, values));
        }
        // A try without throwing operations, or an overriding finalizer, can leave a
        // handler unreachable. Remove it before SSA construction examines its local uses.
        pliron::opts::simplify_cfg::remove_blocks_inside_region(
            region,
            ctx,
            &mut pliron::irbuild::rewriter::IRRewriter::<DummyListener>::default(),
        );
        Ok(func)
    }

    /// `setTimeout(delay?, value?)` from `node:timers/promises`.
    fn timer(&mut self, ctx: &mut Context, args: &[Expr]) -> Result<Value> {
        ensure!(
            args.len() <= 2,
            "setTimeout options (AbortSignal, ref) are not supported yet"
        );
        let delay = match args.first() {
            Some(delay) => {
                let value = self.expression(ctx, delay)?;
                let value = self.arithmetic(ctx, value);
                ensure!(
                    kind(ctx, value)? == Kind::Number,
                    "setTimeout delay must be a number"
                );
                value
            }
            None => self.number(ctx, 1.0),
        };
        let value = match args.get(1) {
            Some(value) => {
                let value = self.expression(ctx, value)?;
                self.defined(ctx, value)?
            }
            None => {
                let number = FP64Type::get(ctx).into();
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number))
            }
        };
        Ok(self.emit_value(ctx, |ctx| TimerOp::new(ctx, delay, value)))
    }

    /// Results of falling off the end or `return;`: an async function settles `undefined`.
    fn implicit_results(&mut self, ctx: &mut Context) -> Vec<Value> {
        self.results
            .clone()
            .into_iter()
            .map(|ty| match optional_value(ctx, ty) {
                Some(inner) if self.is_async => {
                    self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, inner))
                }
                _ => self.emit_value(ctx, |ctx| UndefOp::new(ctx, ty)),
            })
            .collect()
    }

    // ── IR plumbing ────────────────────────────────────────────────────────

    fn append(&mut self, ctx: &mut Context, op: &dyn Op) {
        self.ins.append_op(ctx, op);
        self.guard_exception(ctx, op);
    }

    fn append_value<T: Op + OneResultInterface>(&mut self, ctx: &mut Context, op: &T) -> Value {
        self.append(ctx, op);
        op.get_result(ctx)
    }

    fn emit<T: Op>(&mut self, ctx: &mut Context, make: impl FnOnce(&mut Context) -> T) -> T {
        let op = make(ctx);
        self.append(ctx, &op);
        op
    }

    fn emit_value<T: Op + OneResultInterface>(
        &mut self,
        ctx: &mut Context,
        make: impl FnOnce(&mut Context) -> T,
    ) -> Value {
        let op = self.emit(ctx, make);
        op.get_result(ctx)
    }

    fn new_block(&mut self, ctx: &mut Context, label: &str) -> Ptr<BasicBlock> {
        self.labels += 1;
        let name = Identifier::try_from(format!("{label}{}", self.labels)).expect("label");
        let block = BasicBlock::new(ctx, Some(name), vec![]);
        block.insert_at_back(self.region, ctx);
        block
    }

    fn switch_to(&mut self, block: Ptr<BasicBlock>) {
        self.block = block;
        self.ins.set_insertion_point_to_block_end(block);
    }

    fn terminated(&self, ctx: &Context) -> bool {
        self.block.deref(ctx).get_terminator(ctx).is_some()
    }

    /// Branch to `target` unless the current block already ended (e.g. with `return`).
    fn jump(&mut self, ctx: &mut Context, target: Ptr<BasicBlock>) {
        if !self.terminated(ctx) {
            self.emit(ctx, |ctx| BrOp::new(ctx, target, vec![]));
        }
    }

    fn slot(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        self.emit_value(ctx, |ctx| LocalOp::new(ctx, ty))
    }

    fn store(&mut self, ctx: &mut Context, slot: Value, value: Value) {
        self.emit(ctx, |ctx| StoreOp::new(ctx, slot, value));
    }

    fn load(&mut self, ctx: &mut Context, slot: Value) -> Result<Value> {
        let op = LoadOp::new(ctx, slot)?;
        Ok(self.append_value(ctx, &op))
    }

    fn local(&self, id: LocalId) -> Result<Value> {
        self.locals.get(&id).copied().ok_or_else(|| {
            anyhow!(
                "unsupported reference to local {id} (globals and captures are not supported yet)"
            )
        })
    }

    fn number(&mut self, ctx: &mut Context, value: f64) -> Value {
        let op = ConstantOp::new(ctx, Box::new(FPDoubleAttr::from(value)));
        self.append_value(ctx, &op)
    }

    fn boolean(&mut self, ctx: &mut Context, value: bool) -> Value {
        let ty = IntegerType::get(ctx, 1, Signedness::Signless);
        let attr = IntegerAttr::new(ty, APInt::from_u64(u64::from(value), bw(1)));
        let op = ConstantOp::new(ctx, Box::new(attr));
        self.append_value(ctx, &op)
    }

    fn binary(&mut self, ctx: &mut Context, kind: BinaryKind, lhs: Value, rhs: Value) -> Value {
        self.emit_value(ctx, |ctx| MirBinary::new(ctx, kind, lhs, rhs))
    }

    fn compare(&mut self, ctx: &mut Context, kind: CompareKind, lhs: Value, rhs: Value) -> Value {
        self.emit_value(ctx, |ctx| MirCompare::new(ctx, kind, lhs, rhs))
    }

    fn condition(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        ensure!(
            kind(ctx, value)? == Kind::Boolean,
            "conditions must be boolean (implicit truthiness is not supported yet)"
        );
        Ok(value)
    }

    fn branch(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        then: Ptr<BasicBlock>,
        otherwise: Ptr<BasicBlock>,
    ) {
        self.emit(ctx, |ctx| {
            CondBrOp::new(ctx, condition, then, vec![], otherwise, vec![])
        });
    }

    // ── Statements ─────────────────────────────────────────────────────────

    fn statements(&mut self, ctx: &mut Context, statements: &[Stmt]) -> Result<()> {
        for statement in statements {
            if self.terminated(ctx) {
                break;
            }
            self.statement(ctx, statement)?;
        }
        Ok(())
    }

    fn statement(&mut self, ctx: &mut Context, statement: &Stmt) -> Result<()> {
        match statement {
            Stmt::Let { id, ty, init, .. } => {
                // Perry HIR leaves some bindings untyped (`Any`); those take the initializer's
                // type. An explicit type the subset cannot represent is an error, not a fallback.
                let declared = match ty {
                    HirType::Any | HirType::Unknown => None,
                    ty => Some(self.shapes.value_type(ctx, ty)?),
                };
                let init = init
                    .as_ref()
                    .map(|e| self.expression_with(ctx, e, declared))
                    .transpose()?;
                // A binding initialized with `T | undefined` (an out-of-bounds read, R8) holds
                // `undefined` at runtime even when declared `T`, as in ECMAScript.
                let ty = match (declared, init) {
                    (Some(ty), Some(value))
                        if optional_value(ctx, value.get_type(ctx)) == Some(ty) =>
                    {
                        value.get_type(ctx)
                    }
                    (Some(ty), _) => ty,
                    (None, Some(value)) => value.get_type(ctx),
                    (None, None) => bail!("an untyped binding needs an initializer"),
                };
                let slot = self.entry_slot(ctx, ty);
                if let Some(value) = init {
                    let value = self.fit(ctx, value, ty)?;
                    self.store(ctx, slot, value);
                }
                self.locals.insert(*id, slot);
            }
            Stmt::Expr(expr) => {
                self.effect(ctx, expr)?;
            }
            Stmt::Return(value) => {
                let values = match value {
                    Some(expr) => {
                        let expected = self
                            .results
                            .first()
                            .copied()
                            .context("a void function returns a value")?;
                        let value = self.expression_with(ctx, expr, Some(expected))?;
                        vec![self.fit(ctx, value, expected)?]
                    }
                    None => self.implicit_results(ctx),
                };
                ensure!(
                    values.iter().map(|v| v.get_type(ctx)).collect::<Vec<_>>() == self.results,
                    "return value type differs from the declared result"
                );
                self.run_cleanups(ctx, 0)?;
                if !self.terminated(ctx) {
                    self.emit(ctx, |ctx| ReturnOp::new(ctx, values));
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let condition = self.condition(ctx, condition)?;
                let then = self.new_block(ctx, "then");
                let join = self.new_block(ctx, "join");
                let otherwise = if else_branch.is_some() {
                    self.new_block(ctx, "else")
                } else {
                    join
                };
                self.branch(ctx, condition, then, otherwise);
                self.switch_to(then);
                self.statements(ctx, then_branch)?;
                self.jump(ctx, join);
                if let Some(else_branch) = else_branch {
                    self.switch_to(otherwise);
                    self.statements(ctx, else_branch)?;
                    self.jump(ctx, join);
                }
                self.switch_to(join);
            }
            Stmt::While { condition, body } => {
                self.loop_(ctx, None, Some(condition), None, body, false)?
            }
            Stmt::DoWhile { body, condition } => {
                self.loop_(ctx, None, Some(condition), None, body, true)?
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => self.loop_(
                ctx,
                init.as_deref(),
                condition.as_ref(),
                update.as_ref(),
                body,
                false,
            )?,
            Stmt::Throw(expr) => {
                let value = self.expression(ctx, expr)?;
                let value = self.box_exception(ctx, value)?;
                let status = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, value));
                self.propagate(ctx, status);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                self.try_statement(ctx, body, catch.as_ref(), finally.as_deref())?;
            }
            Stmt::Break | Stmt::Continue => {
                let target = self
                    .loops
                    .last()
                    .map(|l| {
                        if matches!(statement, Stmt::Break) {
                            l.break_to
                        } else {
                            l.continue_to
                        }
                    })
                    .context("break/continue outside a loop")?;
                let depth = self.loops.last().unwrap().cleanup_depth;
                self.run_cleanups(ctx, depth)?;
                self.jump(ctx, target);
            }
            other => bail!(
                "MIR backend does not support statement {} yet",
                statement_name(other)
            ),
        }
        Ok(())
    }

    /// `for (init; condition; update) body`, `while`, and `do … while`.
    fn loop_(
        &mut self,
        ctx: &mut Context,
        init: Option<&Stmt>,
        condition: Option<&Expr>,
        update: Option<&Expr>,
        body: &[Stmt],
        body_first: bool,
    ) -> Result<()> {
        if let Some(init) = init {
            self.statement(ctx, init)?;
        }
        let header = self.new_block(ctx, "header");
        let body_block = self.new_block(ctx, "body");
        let latch = self.new_block(ctx, "latch");
        let exit = self.new_block(ctx, "exit");
        self.jump(ctx, if body_first { body_block } else { header });

        self.switch_to(header);
        match condition {
            Some(condition) => {
                let condition = self.condition(ctx, condition)?;
                self.branch(ctx, condition, body_block, exit);
            }
            None => self.jump(ctx, body_block),
        }

        self.switch_to(body_block);
        self.loops.push(Loop {
            cleanup_depth: self.cleanups.len(),
            continue_to: latch,
            break_to: exit,
        });
        let lowered = self.statements(ctx, body);
        self.loops.pop();
        lowered?;
        self.jump(ctx, latch);

        self.switch_to(latch);
        if let Some(update) = update {
            self.effect(ctx, update)?;
        }
        self.jump(ctx, header);
        self.switch_to(exit);
        Ok(())
    }

    /// Lower an expression evaluated for its effects; void calls are allowed here.
    fn effect(&mut self, ctx: &mut Context, expr: &Expr) -> Result<()> {
        if let Expr::ProcessExit(code) = expr {
            match code.as_deref() {
                None | Some(Expr::Undefined | Expr::Null) => {
                    self.intrinsic(ctx, Intrinsic::ProcessExitCurrent, vec![]);
                }
                Some(code) => {
                    let value = self.expression(ctx, code)?;
                    ensure!(
                        kind(ctx, value)? == Kind::Number,
                        "process.exit currently requires a numeric code"
                    );
                    self.intrinsic(ctx, Intrinsic::ProcessExit, vec![value]);
                }
            }
        } else if let Expr::Call { callee, args, .. } = expr {
            self.call(ctx, callee, args)?;
        } else {
            self.expression(ctx, expr)?;
        }
        Ok(())
    }

    // ── Expressions ────────────────────────────────────────────────────────

    fn expression(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        self.expression_with(ctx, expr, None)
    }

    /// Lower `expr`; `expected` types literals whose own type is not determined by their
    /// operands (empty arrays, object literals widened to a declared shape).
    fn expression_with(
        &mut self,
        ctx: &mut Context,
        expr: &Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        Ok(match expr {
            Expr::Number(value) => self.number(ctx, *value),
            Expr::Integer(value) => self.number(ctx, *value as f64),
            Expr::Bool(value) => self.boolean(ctx, *value),
            Expr::String(text) => {
                let op = StringConstOp::new(ctx, text);
                self.append_value(ctx, &op)
            }
            Expr::ErrorNew(message) => {
                self.new_error(ctx, perry_mir::attrs::ErrorKind::Any, message.as_deref())?
            }
            Expr::TypeErrorNew(message) => {
                self.new_error(ctx, perry_mir::attrs::ErrorKind::TypeError, Some(message))?
            }
            Expr::RangeErrorNew(message) => {
                self.new_error(ctx, perry_mir::attrs::ErrorKind::RangeError, Some(message))?
            }
            Expr::ErrorMessage(value) => {
                let value = self.expression(ctx, value)?;
                ensure!(
                    kind(ctx, value)? == Kind::Exception,
                    "message access needs an Error"
                );
                self.emit_value(ctx, |ctx| ExceptionMessageOp::new(ctx, value))
            }
            Expr::InstanceOf {
                expr,
                ty,
                ty_expr: None,
            } => {
                let value = self.expression(ctx, expr)?;
                let error = match ty.as_str() {
                    "Error" => perry_mir::attrs::ErrorKind::Any,
                    "TypeError" => perry_mir::attrs::ErrorKind::TypeError,
                    "RangeError" => perry_mir::attrs::ErrorKind::RangeError,
                    _ => bail!("instanceof {ty} is not supported yet"),
                };
                let value = self.box_exception(ctx, value)?;
                self.emit_value(ctx, |ctx| ExceptionIsOp::new(ctx, error, value))
            }
            Expr::LocalGet(id) => {
                let slot = self.local(*id)?;
                self.load(ctx, slot)?
            }
            Expr::LocalSet(id, value) => {
                let slot = self.local(*id)?;
                let slot_type = self.slot_type(ctx, slot)?;
                let value = self.expression_with(ctx, value, Some(slot_type))?;
                let value = self.fit(ctx, value, slot_type)?;
                self.store(ctx, slot, value);
                value
            }
            Expr::Update { id, op, prefix } => {
                let slot = self.local(*id)?;
                let old = self.load(ctx, slot)?;
                let old = self.arithmetic(ctx, old);
                ensure!(kind(ctx, old)? == Kind::Number, "++/-- needs a number");
                let one = self.number(ctx, 1.0);
                let kind = match op {
                    UpdateOp::Increment => BinaryKind::Add,
                    UpdateOp::Decrement => BinaryKind::Sub,
                };
                let new = self.binary(ctx, kind, old, one);
                let slot_type = self.slot_type(ctx, slot)?;
                let stored = self.fit(ctx, new, slot_type)?;
                self.store(ctx, slot, stored);
                if *prefix { new } else { old }
            }
            Expr::Binary { op, left, right } => {
                let lhs = self.expression(ctx, left)?;
                let lhs = self.arithmetic(ctx, lhs);
                let rhs = self.expression(ctx, right)?;
                let rhs = self.arithmetic(ctx, rhs);
                match (op, kind(ctx, lhs)?, kind(ctx, rhs)?) {
                    (BinaryOp::Add, Kind::String, Kind::String) => {
                        self.emit_value(ctx, |ctx| StringConcatOp::new(ctx, lhs, rhs))
                    }
                    (op, Kind::Number, Kind::Number) => {
                        let kind = match op {
                            BinaryOp::Add => BinaryKind::Add,
                            BinaryOp::Sub => BinaryKind::Sub,
                            BinaryOp::Mul => BinaryKind::Mul,
                            BinaryOp::Div => BinaryKind::Div,
                            BinaryOp::Mod => BinaryKind::Rem,
                            other => bail!("MIR backend does not support operator {other:?} yet"),
                        };
                        self.binary(ctx, kind, lhs, rhs)
                    }
                    (op, _, _) => {
                        bail!("operator {op:?} on these operand types is not supported yet")
                    }
                }
            }
            Expr::Compare { op, left, right } => {
                if let (CompareOp::Eq | CompareOp::Ne, Some(other)) = (
                    op,
                    match (&**left, &**right) {
                        (Expr::Undefined, other) | (other, Expr::Undefined) => Some(other),
                        _ => None,
                    },
                ) {
                    // `x === undefined`: a definedness test; always false for defined types.
                    let value = self.expression(ctx, other)?;
                    let undefined = if self.is_optional(ctx, value) {
                        self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value))
                    } else {
                        self.boolean(ctx, false)
                    };
                    return Ok(if matches!(op, CompareOp::Eq) {
                        undefined
                    } else {
                        let no = self.boolean(ctx, false);
                        self.compare(ctx, CompareKind::Eq, undefined, no)
                    });
                }
                let lhs = self.expression(ctx, left)?;
                let rhs = self.expression(ctx, right)?;
                if matches!(op, CompareOp::Eq | CompareOp::Ne)
                    && (lhs.get_type(ctx).deref(ctx).is::<ExceptionType>()
                        || rhs.get_type(ctx).deref(ctx).is::<ExceptionType>())
                {
                    let lhs = self.box_exception(ctx, lhs)?;
                    let rhs = self.box_exception(ctx, rhs)?;
                    let kind = if matches!(op, CompareOp::Eq) {
                        CompareKind::Eq
                    } else {
                        CompareKind::Ne
                    };
                    return Ok(self.compare(ctx, kind, lhs, rhs));
                }
                if matches!(op, CompareOp::Eq | CompareOp::Ne)
                    && (self.is_optional(ctx, lhs) || self.is_optional(ctx, rhs))
                {
                    // Strict equality with `undefined` in play: compare as `T | undefined`.
                    let kind = if matches!(op, CompareOp::Eq) {
                        CompareKind::Eq
                    } else {
                        CompareKind::Ne
                    };
                    let lhs = self.optional(ctx, lhs);
                    let rhs = self.optional(ctx, rhs);
                    ensure!(
                        lhs.get_type(ctx) == rhs.get_type(ctx),
                        "comparison of different types is not supported"
                    );
                    return Ok(self.compare(ctx, kind, lhs, rhs));
                }
                let lhs = self.arithmetic(ctx, lhs);
                let rhs = self.arithmetic(ctx, rhs);
                let (left_kind, right_kind) = (kind(ctx, lhs)?, kind(ctx, rhs)?);
                ensure!(
                    left_kind == right_kind,
                    "comparison of different types is not supported"
                );
                let kind = match op {
                    CompareOp::Eq => CompareKind::Eq,
                    CompareOp::Ne => CompareKind::Ne,
                    CompareOp::Lt => CompareKind::Lt,
                    CompareOp::Le => CompareKind::Le,
                    CompareOp::Gt => CompareKind::Gt,
                    CompareOp::Ge => CompareKind::Ge,
                    CompareOp::LooseEq | CompareOp::LooseNe => {
                        bail!("loose equality is not supported; use === or !==")
                    }
                };
                ensure!(
                    left_kind == Kind::Number || matches!(kind, CompareKind::Eq | CompareKind::Ne),
                    "ordered comparison of booleans is not supported"
                );
                self.compare(ctx, kind, lhs, rhs)
            }
            Expr::Logical { op, left, right } => {
                // Short-circuit through a result slot; mem2reg turns it into a block argument.
                ensure!(
                    !matches!(op, LogicalOp::Coalesce),
                    "?? is not supported yet"
                );
                let lhs = self.condition(ctx, left)?;
                let result = self.entry_slot(ctx, lhs.get_type(ctx));
                self.store(ctx, result, lhs);
                let rhs_block = self.new_block(ctx, "rhs");
                let join = self.new_block(ctx, "join");
                match op {
                    LogicalOp::And => self.branch(ctx, lhs, rhs_block, join),
                    _ => self.branch(ctx, lhs, join, rhs_block),
                }
                self.switch_to(rhs_block);
                let rhs = self.condition(ctx, right)?;
                self.store(ctx, result, rhs);
                self.jump(ctx, join);
                self.switch_to(join);
                self.load(ctx, result)?
            }
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
            } => {
                let condition = self.condition(ctx, condition)?;
                let then = self.new_block(ctx, "then");
                let otherwise = self.new_block(ctx, "else");
                let join = self.new_block(ctx, "join");
                self.branch(ctx, condition, then, otherwise);
                self.switch_to(then);
                let then_value = self.expression(ctx, then_expr)?;
                let ty = then_value.get_type(ctx);
                // The slot is created in the entry block so it dominates both arms.
                let result = self.entry_slot(ctx, ty);
                self.store(ctx, result, then_value);
                self.jump(ctx, join);
                self.switch_to(otherwise);
                let else_value = self.expression(ctx, else_expr)?;
                let else_value = self.fit(ctx, else_value, ty)?;
                self.store(ctx, result, else_value);
                self.jump(ctx, join);
                self.switch_to(join);
                self.load(ctx, result)?
            }
            Expr::Unary { op, operand } => {
                let value = self.expression(ctx, operand)?;
                let value = self.arithmetic(ctx, value);
                match (op, kind(ctx, value)?) {
                    (UnaryOp::Neg, Kind::Number) => {
                        // `0 - x` loses the sign of zero; `-1 * x` keeps IEEE negation.
                        let minus_one = self.number(ctx, -1.0);
                        self.binary(ctx, BinaryKind::Mul, minus_one, value)
                    }
                    (UnaryOp::Pos, Kind::Number) => value,
                    (UnaryOp::Not, Kind::Boolean) => {
                        let no = self.boolean(ctx, false);
                        self.compare(ctx, CompareKind::Eq, value, no)
                    }
                    (op, _) => bail!("unary {op:?} on this type is not supported yet"),
                }
            }
            Expr::Call { callee, args, .. } => self
                .call(ctx, callee, args)?
                .context("a void call has no value")?,
            Expr::Await(promise) => {
                ensure!(self.is_async, "await is only valid in async functions");
                let promise = self.expression(ctx, promise)?;
                ensure!(
                    TypedHandle::<PromiseType>::from_handle(promise.get_type(ctx), ctx).is_ok(),
                    "await of a non-promise value is not supported yet"
                );
                let op = AwaitOp::new(ctx, promise).map_err(|e| anyhow!("{}", e.disp(ctx)))?;
                self.append_value(ctx, &op)
            }
            Expr::PerformanceNow => self
                .intrinsic(ctx, Intrinsic::MonotonicNow, vec![])
                .unwrap(),
            Expr::DateNow => self.intrinsic(ctx, Intrinsic::DateNow, vec![]).unwrap(),
            Expr::EnvGet(name) => {
                let name = self.emit_value(ctx, |ctx| StringConstOp::new(ctx, name));
                self.intrinsic(ctx, Intrinsic::EnvGet, vec![name]).unwrap()
            }
            Expr::ObjectKeys(object) if matches!(object.as_ref(), Expr::ProcessEnv) => {
                self.intrinsic(ctx, Intrinsic::EnvKeys, vec![]).unwrap()
            }
            Expr::ProcessArgv => self.intrinsic(ctx, Intrinsic::Arguments, vec![]).unwrap(),
            Expr::PropertyGet {
                object, property, ..
            } => {
                let value = self.expression(ctx, object)?;
                // Property access on `undefined` throws TypeError, in ECMAScript and here.
                let value = self.defined(ctx, value)?;
                match (kind(ctx, value)?, property.as_str()) {
                    (Kind::String, "length") => {
                        let length = self.emit_value(ctx, |ctx| StringLenOp::new(ctx, value));
                        self.emit_value(ctx, |ctx| {
                            ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
                        })
                    }
                    (Kind::Array, "length") => {
                        let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, value));
                        self.emit_value(ctx, |ctx| {
                            ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
                        })
                    }
                    (Kind::Exception, "message") => {
                        self.emit_value(ctx, |ctx| ExceptionMessageOp::new(ctx, value))
                    }
                    (Kind::Record, field) => {
                        let field = identifier(field)?;
                        let op = FieldGetOp::new(ctx, value, field.clone()).map_err(|_| {
                            anyhow!(
                                "property {field} does not exist on this shape (subset rule R1)"
                            )
                        })?;
                        self.append_value(ctx, &op)
                    }
                    (_, property) => bail!("property {property} is not supported on this type yet"),
                }
            }
            Expr::PropertySet {
                object,
                property,
                value,
            } => self.set_field(ctx, object, property, value)?,
            Expr::PutValueSet {
                target, key, value, ..
            } => match key.as_ref() {
                Expr::String(field) => self.set_field(ctx, target, field, value)?,
                index => self.set_index(ctx, target, index, value)?,
            },
            Expr::New {
                class_name, args, ..
            } => {
                let names = self
                    .shapes
                    .literal_fields(class_name)
                    .with_context(|| format!("class {class_name} is not supported yet"))?;
                ensure!(
                    names.len() == args.len(),
                    "object literal arity differs from its shape"
                );
                self.record_literal(ctx, &names, args, expected)?
            }
            Expr::Array(items) => {
                let element =
                    match expected.map(|ty| TypedHandle::<ArrayType>::from_handle(ty, ctx)) {
                        Some(Ok(array)) => Some(array.deref(ctx).element()),
                        _ => None,
                    };
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    let value = self.expression_with(ctx, item, element)?;
                    let value = match element {
                        Some(element) => self.fit(ctx, value, element)?,
                        None => self.defined(ctx, value)?,
                    };
                    values.push(value);
                }
                let element = match (element, values.first()) {
                    (Some(element), _) => element,
                    (None, Some(first)) => first.get_type(ctx),
                    (None, None) => bail!("an empty array literal needs a declared element type"),
                };
                ensure!(
                    values.iter().all(|v| v.get_type(ctx) == element),
                    "array literals must be homogeneous"
                );
                self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, values))
            }
            Expr::ArrayPush {
                array_id, value, ..
            } => {
                let slot = self.local(*array_id)?;
                let array = self.load(ctx, slot)?;
                let element = self.array_element(ctx, array)?;
                let value = self.expression_with(ctx, value, Some(element))?;
                let value = self.fit(ctx, value, element)?;
                self.emit(ctx, |ctx| ArrayPushOp::new(ctx, array, value));
                let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, array));
                self.emit_value(ctx, |ctx| {
                    ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
                })
            }
            Expr::IndexGet { object, index } => {
                let array = self.expression(ctx, object)?;
                let array = self.defined(ctx, array)?;
                ensure!(
                    kind(ctx, array)? == Kind::Array,
                    "indexing is only supported on arrays so far"
                );
                let index = self.index(ctx, index)?;
                let op = ArrayGetOp::new(ctx, array, index)?;
                self.append_value(ctx, &op)
            }
            Expr::IndexSet {
                object,
                index,
                value,
            } => self.set_index(ctx, object, index, value)?,
            other => bail!(
                "MIR backend does not support expression {} yet",
                expression_name(other)
            ),
        })
    }

    /// A slot inserted at the start of the entry block, dominating every use.
    fn entry_slot(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        let entry = self.region.deref(ctx).get_head().expect("entry block");
        let op = LocalOp::new(ctx, ty);
        let mut at_entry = IRInserter::<DummyListener>::new_at_block_start(entry);
        at_entry.append_op(ctx, &op);
        op.get_result(ctx)
    }

    fn slot_type(&self, ctx: &Context, slot: Value) -> Result<TypeHandle> {
        Ok(
            TypedHandle::<perry_mir::types::SlotType>::from_handle(slot.get_type(ctx), ctx)
                .map_err(|_| anyhow!("not a slot"))?
                .deref(ctx)
                .value(),
        )
    }

    fn array_element(&self, ctx: &Context, array: Value) -> Result<TypeHandle> {
        Ok(
            TypedHandle::<ArrayType>::from_handle(array.get_type(ctx), ctx)
                .map_err(|_| anyhow!("expected an array"))?
                .deref(ctx)
                .element(),
        )
    }

    /// An ECMAScript number used as an array index (R8: reads miss with `undefined`).
    fn index(&mut self, ctx: &mut Context, index: &Expr) -> Result<Value> {
        let index = self.expression(ctx, index)?;
        let index = self.arithmetic(ctx, index);
        ensure!(
            kind(ctx, index)? == Kind::Number,
            "array indices must be numbers"
        );
        Ok(index)
    }

    fn is_optional(&self, ctx: &Context, value: Value) -> bool {
        optional_value(ctx, value.get_type(ctx)).is_some()
    }

    /// ECMAScript coercion of `T | undefined` for arithmetic, ordering and concatenation:
    /// `undefined` becomes `NaN` or `"undefined"`. Other values pass through.
    fn arithmetic(&mut self, ctx: &mut Context, value: Value) -> Value {
        let Some(inner) = optional_value(ctx, value.get_type(ctx)) else {
            return value;
        };
        if inner.deref(ctx).is::<pliron::builtin::types::FP64Type>() {
            self.emit_value(ctx, |ctx| ToNumberOp::new(ctx, value))
        } else if inner.deref(ctx).is::<StringType>() {
            self.emit_value(ctx, |ctx| ToStringOp::new(ctx, value))
        } else {
            value
        }
    }

    /// The defined value of `T | undefined`; throws `TypeError` on `undefined`.
    fn defined(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if !self.is_optional(ctx, value) {
            return Ok(value);
        }
        let op = UnwrapOp::new(ctx, value)?;
        Ok(self.append_value(ctx, &op))
    }

    /// View `value` as `T | undefined`.
    fn optional(&mut self, ctx: &mut Context, value: Value) -> Value {
        if self.is_optional(ctx, value) {
            value
        } else {
            self.emit_value(ctx, |ctx| WrapOp::new(ctx, value))
        }
    }

    /// Convert `value` to the type of a typed boundary: wrap into `T | undefined`, or unwrap
    /// (throwing `TypeError` on `undefined`) where `T` is required.
    fn fit(&mut self, ctx: &mut Context, value: Value, target: TypeHandle) -> Result<Value> {
        let ty = value.get_type(ctx);
        if ty == target {
            Ok(value)
        } else if optional_value(ctx, target) == Some(ty) {
            Ok(self.optional(ctx, value))
        } else if optional_value(ctx, ty) == Some(target) {
            self.defined(ctx, value)
        } else {
            bail!("value type differs from the expected type")
        }
    }

    /// `array[index] = value`; computed keys on records are rejected (subset rule R1).
    fn set_index(
        &mut self,
        ctx: &mut Context,
        object: &Expr,
        index: &Expr,
        value: &Expr,
    ) -> Result<Value> {
        let array = self.expression(ctx, object)?;
        let array = self.defined(ctx, array)?;
        ensure!(
            kind(ctx, array)? == Kind::Array,
            "computed property keys are only supported on arrays (subset rule R1)"
        );
        let element = self.array_element(ctx, array)?;
        let index = self.index(ctx, index)?;
        let value = self.expression_with(ctx, value, Some(element))?;
        let value = self.fit(ctx, value, element)?;
        self.emit(ctx, |ctx| ArraySetOp::new(ctx, array, index, value));
        Ok(value)
    }

    fn set_field(
        &mut self,
        ctx: &mut Context,
        object: &Expr,
        field: &str,
        value: &Expr,
    ) -> Result<Value> {
        let record = self.expression(ctx, object)?;
        let record = self.defined(ctx, record)?;
        ensure!(
            kind(ctx, record)? == Kind::Record,
            "property writes are only supported on records so far"
        );
        let field = identifier(field)?;
        let field_type = TypedHandle::<RecordType>::from_handle(record.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected a record"))?
            .deref(ctx)
            .field(&field)
            .map(|(_, ty)| ty)
            .ok_or_else(|| {
                anyhow!("property {field} does not exist on this shape (subset rule R1)")
            })?;
        let value = self.expression_with(ctx, value, Some(field_type))?;
        let value = self.fit(ctx, value, field_type)?;
        self.emit(ctx, |ctx| FieldSetOp::new(ctx, record, field, value));
        Ok(value)
    }

    /// `{ a: …, b: … }`: evaluate fields in source order, allocate in record order.
    fn record_literal(
        &mut self,
        ctx: &mut Context,
        names: &[String],
        args: &[Expr],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let mut wanted: Vec<&str> = names.iter().map(String::as_str).collect();
        wanted.sort_unstable();
        let expected = expected
            .and_then(|ty| TypedHandle::<RecordType>::from_handle(ty, ctx).ok())
            .filter(|record| {
                record.deref(ctx).fields().is_ok_and(|fields| {
                    fields
                        .iter()
                        .map(|(n, _)| n.to_string())
                        .collect::<Vec<_>>()
                        == wanted
                })
            });
        let mut values = Vec::with_capacity(args.len());
        for (name, arg) in names.iter().zip(args) {
            let field_type = match (expected, identifier(name)) {
                (Some(record), Ok(field)) => record.deref(ctx).field(&field).map(|(_, ty)| ty),
                _ => None,
            };
            let value = self.expression_with(ctx, arg, field_type)?;
            let value = match field_type {
                Some(field_type) => self.fit(ctx, value, field_type)?,
                None => self.defined(ctx, value)?,
            };
            values.push((name.clone(), value));
        }
        // With a matching expected type the fields were fitted to it; that includes recursive
        // records, which no literal's own structure can name.
        let record = match expected {
            Some(record) => record,
            None => {
                let fields = values
                    .iter()
                    .map(|(n, v)| (n.clone(), v.get_type(ctx)))
                    .collect();
                self.shapes.mir_record(ctx, fields)?
            }
        };
        values.sort_by(|a, b| a.0.cmp(&b.0));
        let operands = values.into_iter().map(|(_, v)| v).collect();
        Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, operands)))
    }

    fn intrinsic(&mut self, ctx: &mut Context, kind: Intrinsic, args: Vec<Value>) -> Option<Value> {
        let op = IntrinsicOp::new(ctx, kind, args);
        self.append(ctx, &op);
        let op = op.get_operation().deref(ctx);
        (op.get_num_results() == 1).then(|| op.get_result(0))
    }

    fn call(&mut self, ctx: &mut Context, callee: &Expr, args: &[Expr]) -> Result<Option<Value>> {
        if let Expr::ExternFuncRef { name, .. } = callee {
            match name.as_str() {
                "js_process_exit_code_get" => {
                    ensure!(args.is_empty(), "exitCode getter takes no arguments");
                    return Ok(self.intrinsic(ctx, Intrinsic::ExitCodeGet, vec![]));
                }
                "js_process_exit_code_set" => {
                    ensure!(args.len() == 1, "exitCode setter takes one argument");
                    if matches!(args[0], Expr::Undefined | Expr::Null) {
                        return Ok(self.intrinsic(ctx, Intrinsic::ExitCodeClear, vec![]));
                    }
                    let value = self.expression(ctx, &args[0])?;
                    ensure!(
                        kind(ctx, value)? == Kind::Number,
                        "process.exitCode currently requires a numeric code"
                    );
                    return Ok(self.intrinsic(ctx, Intrinsic::ExitCodeSet, vec![value]));
                }
                _ => {}
            }
        }
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
            && matches!(object.as_ref(), Expr::GlobalGet(0))
            && matches!(property.as_str(), "log" | "error")
        {
            ensure!(
                args.len() == 1,
                "console.{property} currently requires one string argument"
            );
            let value = self.expression(ctx, &args[0])?;
            let value = self.arithmetic(ctx, value);
            ensure!(
                kind(ctx, value)? == Kind::String,
                "console.{property} currently requires a string"
            );
            let newline = self.emit_value(ctx, |ctx| StringConstOp::new(ctx, "\n"));
            let text = self.emit_value(ctx, |ctx| StringConcatOp::new(ctx, value, newline));
            let kind = if property == "log" {
                Intrinsic::Stdout
            } else {
                Intrinsic::Stderr
            };
            return Ok(self.intrinsic(ctx, kind, vec![text]));
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && self.timers.contains(name)
        {
            return self.timer(ctx, args).map(Some);
        }
        let Expr::FuncRef(id) = callee else {
            bail!("only direct calls to module functions are supported yet");
        };
        let signature = self
            .signatures
            .get(id)
            .context("call to an unknown function")?;
        let (name, params, results) = (
            signature.name.clone(),
            signature.params.clone(),
            signature.results.clone(),
        );
        ensure!(
            args.len() == params.len(),
            "call to {name} with {} arguments, expected {}",
            args.len(),
            params.len()
        );
        let values = args
            .iter()
            .zip(&params)
            .map(|(arg, param)| {
                let value = self.expression_with(ctx, arg, Some(*param))?;
                self.fit(ctx, value, *param)
            })
            .collect::<Result<Vec<_>>>()?;
        if signature.is_async {
            let value = results[0];
            let promise = self.emit_value(ctx, |ctx| AsyncCallOp::new(ctx, name, values, value));
            return Ok(Some(promise));
        }
        let op = CallOp::new(ctx, name, values, results);
        self.append(ctx, &op);
        let operation = op.get_operation().deref(ctx);
        Ok((operation.get_num_results() == 1).then(|| operation.get_result(0)))
    }
}

fn statement_name(statement: &Stmt) -> String {
    format!("{statement:?}")
        .split(['(', ' ', '{'])
        .next()
        .unwrap_or("?")
        .to_string()
}

fn expression_name(expr: &Expr) -> String {
    format!("{expr:?}")
        .split(['(', ' ', '{'])
        .next()
        .unwrap_or("?")
        .to_string()
}

#[cfg(test)]
mod tests;
