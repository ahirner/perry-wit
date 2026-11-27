//! Solved inference tables, call-site resolutions, and synthesized helper specs.
use super::super::BitSet;
use super::super::plans::ExprIdx;
use super::super::types::{
    ActivationSpan, CaptureOwner, ClassId, ClassInstId, DispatchArmSpan, FieldLocSpan,
    FuncIdentity, HelperId, Idx, IdxVec, InstanceId, LocId, Pool, TemplateId, Ty, TyKind,
    TypeTable,
};
use super::super::types::{LocalInfo, LocalStorage};
use crate::hir_compat;
use perry_wit_diagnostics::{Diagnostic, Result};
use std::collections::HashMap;

pub(crate) use super::super::types::Receiver;

impl CaptureOwner {
    pub(in crate::engine) fn location(
        mut self,
        instances: &IdxVec<InstanceId, Instance>,
        local: LocalInfo,
        collapsed: Option<LocId>,
    ) -> Option<LocId> {
        let slot = match local.storage {
            LocalStorage::Module(loc) => return Some(loc),
            LocalStorage::Activation(slot) => slot,
        };
        loop {
            match self {
                Self::Instance(id) => {
                    let inst = &instances[id];
                    if inst.template == local.owner {
                        return inst.activation.locations().nth(slot as usize);
                    }
                    self = inst.capture_owner;
                }
                Self::Template(..) => return collapsed,
                Self::None => return None,
            }
        }
    }

    pub(in crate::engine) fn receiver(
        self,
        instances: &IdxVec<InstanceId, Instance>,
    ) -> Option<Receiver> {
        match self {
            Self::Instance(id) => instances[id].receiver,
            Self::Template(_, receiver) => receiver,
            Self::None => None,
        }
    }
}

impl Receiver {
    pub(crate) fn class(self) -> Option<ClassInstId> {
        match self {
            Self::Class(id) => Some(id),
            Self::Object(_) => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Invocation {
    Function,
    Inline,
    Lexical,
}

pub(crate) struct Instance {
    pub(in crate::engine) activation: ActivationSpan,
    pub(crate) is_witness: bool,
    pub(crate) list_result: Option<Ty>,
    pub(crate) template: TemplateId,
    pub(crate) receiver: Option<Receiver>,
    pub(crate) capture_owner: CaptureOwner,
    pub(crate) args: Ty,
    pub(crate) return_loc: LocId,
    pub(crate) partial_record_result: Option<Ty>,
    pub(crate) has_explicit_return: bool,
    pub(crate) caller: Option<InstanceId>,
    pub(crate) call_byte_offset: u32,
    pub(crate) invocation: Invocation,
    pub(crate) emitted_func_id: u32,
    pub(crate) emitted_name: String,
}

impl Instance {
    pub(crate) fn identity(&self) -> FuncIdentity {
        FuncIdentity {
            template: self.template,
            capture_owner: self.capture_owner,
        }
    }
}

pub(crate) struct ClassInst {
    pub(crate) ty: Ty,
    pub(crate) class: ClassId,
    pub(crate) args: Ty,
    pub(crate) field_locs: FieldLocSpan,
    pub(crate) field_init: Option<InstanceId>,
    pub(crate) ctor_inst: Option<InstanceId>,
    pub(crate) emitted_name: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ReactionKind {
    Then,
    Catch,
    Finally,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum FunctionTarget {
    Direct(InstanceId),
    Closure(InstanceId),
    Dispatcher(HelperId),
}

impl FunctionTarget {
    pub(super) fn call(self, receiver: bool) -> CallTarget {
        if receiver {
            CallTarget::Method(self)
        } else {
            CallTarget::Function(self)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CallTarget {
    Function(FunctionTarget),
    Method(FunctionTarget),
    ThrowTypeError,
}

impl CallTarget {
    pub(crate) fn callee(self) -> Option<FunctionTarget> {
        match self {
            Self::Function(f) | Self::Method(f) => Some(f),
            Self::ThrowTypeError => None,
        }
    }

    pub(crate) fn needs_receiver(self) -> bool {
        matches!(self, Self::Method(_))
    }

    pub(crate) fn needs_value(self) -> bool {
        matches!(
            self.callee(),
            Some(FunctionTarget::Closure(_) | FunctionTarget::Dispatcher(_))
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DispatchArm {
    pub(crate) identity: FuncIdentity,
    pub(crate) target: InstanceId,
    pub(crate) receiver: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct PromiseReaction {
    pub(crate) kind: ReactionKind,
    pub(crate) promise_ty: Ty,
    pub(crate) inner_ty: Ty,
    pub(crate) on_fulfilled: Option<(CallTarget, Ty)>,
    pub(crate) on_rejected: Option<(CallTarget, Ty)>,
    pub(crate) ret_ty: Ty,
}

impl PromiseReaction {
    pub(crate) fn arguments(self) -> [Option<(CallTarget, Ty)>; 2] {
        match self.kind {
            ReactionKind::Catch => [self.on_rejected, None],
            ReactionKind::Then | ReactionKind::Finally => [self.on_fulfilled, self.on_rejected],
        }
    }

    pub(crate) fn result(self) -> Ty {
        if self.ret_ty == Ty::BOTTOM {
            Ty::VOID
        } else {
            self.ret_ty
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum HelperSpec {
    Dispatcher {
        result: Ty,
        callee_ty: Ty,
        arms: DispatchArmSpan,
        arg_tys: Ty,
        recv_ty: Option<Ty>,
    },
    PromiseReaction(PromiseReaction),
    TimerRunner {
        schedule: crate::engine::TimerSchedule,
        cb_call: CallTarget,
        inputs: Ty,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperandUse {
    RegexSearch(perry_mir::RegexShape),
    Value(ValueCoercion),
    Callback(FunctionTarget),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueCoercion {
    Zero,
    Tag(FuncIdentity, Ty),
    BoxEnv(FuncIdentity, Ty),
    Widen { from: Ty, to: Ty },
}

impl ValueCoercion {
    pub(crate) fn into_union(
        types: &TypeTable,
        from: Ty,
        to: Ty,
        capture_free: &BitSet,
        escaping: &BitSet,
    ) -> Option<Self> {
        if from == to || !types.is_tagged_func_union(to, capture_free, escaping) {
            return None;
        }
        match types.kind(from) {
            TyKind::Func(id) if types.is_zero_sized_func(from, capture_free) => {
                Some(Self::Tag(id, to))
            }
            TyKind::Func(id) if escaping.contains(id.template.index()) => {
                Some(Self::BoxEnv(id, to))
            }
            TyKind::Union(_)
                if types.is_tagged_func_union(from, capture_free, escaping)
                    && types.func_members(&from).ne(types.func_members(&to)) =>
            {
                Some(Self::Widen { from, to })
            }
            _ => None,
        }
    }

    pub(crate) fn reads_operand(self) -> bool {
        matches!(self, Self::BoxEnv(..) | Self::Widen { .. })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PromiseExecutor {
    pub(crate) call: CallTarget,
    pub(crate) handles: [Option<FuncIdentity>; 2],
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SiteResolution {
    Call(CallTarget),
    FuncTypeOf(Option<Ty>),
    FuncCompare {
        left_ty: Ty,
        right_ty: Ty,
        equal: bool,
        environments: bool,
    },
    Matches {
        identity: FuncIdentity,
        source: Ty,
        left_erased: bool,
        equal: bool,
    },
    ResourceCall(crate::wit::MemberId),
    Static(crate::methods::StaticMethod),
    Builtin(crate::methods::Method),
    Regex(Option<perry_mir::RegexLiteral>),
    ReadMember(InstanceId),
    WriteMember(InstanceId),
    UpdateMember {
        get: Option<InstanceId>,
        set: Option<InstanceId>,
    },
    Callback {
        target: InstanceId,
    },
    ClassNew(ClassInstId),
    Closure(FuncIdentity),
    Environment(super::super::ClosureValue),
    BranchResult(Ty),
    ObjectSpread(Ty),
    TypedArray(Ty),
    ZeroSizedFieldSet,
    CollectionNew(Ty),
    ReadFile {
        text: bool,
    },
    Cell(LocId),
    FoldBool(bool),
    PromiseNew {
        value_loc: LocId,
        resolve_tid: TemplateId,
        reject_tid: TemplateId,
        executor: Option<PromiseExecutor>,
    },
    PromiseReaction {
        result: LocId,
        helper: HelperId,
    },
    TimerStart {
        helper: HelperId,
    },
    TimerClear {
        arg_ty: Option<Ty>,
    },
}

#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct SiteEntry {
    pub(crate) erased: bool,
    pub(crate) resolution: Option<SiteResolution>,
    pub(crate) operand: Option<OperandUse>,
}

pub(crate) type InstanceSites = std::collections::BTreeMap<ExprIdx, SiteEntry>;

impl SiteEntry {
    pub(crate) fn environment(self) -> Option<super::super::ClosureValue> {
        match self.resolution {
            Some(SiteResolution::Environment(value)) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn conversion(self) -> Option<ValueCoercion> {
        match self.operand {
            Some(OperandUse::Value(value)) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn promise(
        sites: &IdxVec<InstanceId, InstanceSites>,
        owner: CaptureOwner,
        site: ExprIdx,
    ) -> Result<LocId> {
        let entry = owner.instance().and_then(|inst| sites[inst].get(&site));
        match entry.and_then(|e| e.resolution.as_ref()) {
            Some(SiteResolution::PromiseNew { value_loc, .. }) => Ok(*value_loc),
            _ => Err(Diagnostic::internal(
                "missing promise constructor for settlement handle",
            )),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HelperEntry {
    pub(crate) spec: HelperSpec,
    pub(crate) byte_offset: u32,
}

pub(crate) enum AdapterResult {
    Declared(Ty),
    Inferred(LocId),
}

pub(crate) enum ExportRoot {
    Function(InstanceId),
    Adapter {
        target: hir_compat::Target<InstanceId>,
        params: Ty,
        result: AdapterResult,
    },
}

pub(crate) struct Solved {
    pub(crate) writes: crate::narrowing::WriteEffects,
    pub(crate) resource_instances: HashMap<u32, ClassInstId>,
    pub(crate) init_inst: InstanceId,
    pub(crate) instances: IdxVec<InstanceId, Instance>,
    pub(crate) class_insts: IdxVec<ClassInstId, ClassInst>,
    pub(crate) locations: IdxVec<LocId, Ty>,
    pub(crate) field_locs: Pool<FieldLocSpan, LocId>,
    pub(crate) dispatch_arms: Pool<DispatchArmSpan, DispatchArm>,
    pub(crate) helpers: IdxVec<HelperId, HelperEntry>,
    pub(in crate::engine) collapsed_locals: IdxVec<super::LocalIdx, Option<LocId>>,
    pub(crate) inst_sites: IdxVec<InstanceId, InstanceSites>,
    pub(crate) live_instances: BitSet,
    pub(crate) live_helpers: BitSet,
    pub(crate) escaping_env_templates: BitSet,

    pub(crate) boxed_locals: BitSet,

    pub(crate) dict_reads: BitSet,
    pub(crate) self_nested_locs: BitSet,
    pub(crate) deletes_array_elements: bool,
}
