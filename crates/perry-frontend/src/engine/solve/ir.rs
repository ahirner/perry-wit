//! Solved inference tables, call-site resolutions, and synthesized helper specs.
use super::super::bitset::BitSet;
use super::super::types::{
    CaptureOwner, ClassId, ClassInstId, DispatchArmSpan, FieldLocSpan, FuncIdentity, HelperId,
    IdxVec, InstanceId, LocId, ParamLocSpan, Pool, TemplateId, Ty,
};
use crate::hir_compat;
use perry_wit_diagnostics::{Diagnostic, Result};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Receiver {
    Class(ClassInstId),
    Object(Ty),
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
    pub(crate) is_witness: bool,
    pub(crate) list_result: Option<Ty>,
    pub(crate) template: TemplateId,
    pub(crate) receiver: Option<Receiver>,
    pub(crate) capture_owner: CaptureOwner,
    pub(crate) args: Ty,
    pub(crate) explicit_type_args: Ty,
    pub(crate) return_loc: LocId,
    pub(crate) partial_record_result: Option<Ty>,
    pub(crate) has_explicit_return: bool,
    pub(crate) param_locs: ParamLocSpan,
    pub(crate) caller: Option<InstanceId>,
    pub(crate) call_byte_offset: u32,
    pub(crate) invocation: Invocation,
    pub(crate) emitted_func_id: u32,
    pub(crate) emitted_name: String,
    pub(crate) emitted_method_name: String,
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
pub(crate) enum CallTarget {
    Direct {
        target: InstanceId,
        pass_env: bool,
        pass_receiver: bool,
    },
    Dispatch {
        helper: HelperId,
        needs_recv: bool,
    },
    ThrowTypeError,
}

impl CallTarget {
    pub(crate) fn needs_receiver(self) -> bool {
        match self {
            Self::Direct { pass_receiver, .. } => pass_receiver,
            Self::Dispatch { needs_recv, .. } => needs_recv,
            Self::ThrowTypeError => false,
        }
    }

    pub(crate) fn needs_value(self) -> bool {
        matches!(
            self,
            Self::Direct { pass_env: true, .. } | Self::Dispatch { .. }
        )
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
    PromiseReaction {
        kind: ReactionKind,
        promise_ty: Ty,
        inner_ty: Ty,
        on_fulfilled: Option<(CallTarget, Ty)>,
        on_rejected: Option<(CallTarget, Ty)>,
        ret_ty: Ty,
    },
    TimerRunner {
        kind: crate::engine::TimerKind,
        timer_ty: Ty,
        cb_call: CallTarget,
        cb_ty: Ty,
        extra_arg_tys: Ty,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FuncCoercion {
    Value(ValueCoercion),
    Callback { target: CallTarget, inputs: Ty },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueCoercion {
    Zero,
    Tag(FuncIdentity, Ty),
    BoxEnv(FuncIdentity, Ty),
    Widen { from: Ty, to: Ty },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PromiseExecutor {
    pub(crate) call: CallTarget,
    pub(crate) handles: [Option<FuncIdentity>; 2],
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum MemberTarget {
    Class(InstanceId),
    Resource(crate::wit::MemberId),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SiteResolution {
    Call {
        target: CallTarget,
        argument_types: Ty,
    },
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
    MemberCall(MemberTarget),
    Callback {
        target: InstanceId,
    },
    ClassNew(ClassInstId),
    Closure(FuncIdentity),
    BranchResult(Ty),
    ObjectSpread(Ty),
    ZeroSizedArray,
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
        pass_first: bool,
        pass_second: bool,
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
    pub(crate) coercion: Option<FuncCoercion>,
}

impl SiteEntry {
    pub(crate) fn conversion(self) -> Option<ValueCoercion> {
        match self.coercion {
            Some(FuncCoercion::Value(value)) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn promise(
        sites: &IdxVec<InstanceId, Vec<Self>>,
        owner: CaptureOwner,
        site: usize,
    ) -> Result<LocId> {
        let entry = owner.instance().and_then(|inst| sites[inst].get(site));
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

#[derive(Clone, Default)]
pub(crate) struct ClassInstMembers {
    pub(crate) methods: Vec<InstanceId>,
    pub(crate) getters: Vec<(usize, InstanceId)>,
    pub(crate) setters: Vec<(usize, InstanceId)>,
}

pub(crate) enum ExportRoot {
    Function(InstanceId),
    Adapter {
        target: hir_compat::Target<InstanceId>,
        params: Ty,
        result: LocId,
    },
}

pub(crate) struct Solved {
    pub(crate) witness_instances: Vec<Option<ExportRoot>>,
    pub(crate) resource_instances: HashMap<u32, ClassInstId>,
    pub(crate) init_inst: InstanceId,
    pub(crate) instances: IdxVec<InstanceId, Instance>,
    pub(crate) class_insts: IdxVec<ClassInstId, ClassInst>,
    pub(crate) locations: IdxVec<LocId, Ty>,
    pub(crate) param_locs: Pool<ParamLocSpan, LocId>,
    pub(crate) field_locs: Pool<FieldLocSpan, LocId>,
    pub(crate) dispatch_arms: Pool<DispatchArmSpan, (FuncIdentity, InstanceId)>,
    pub(crate) helpers: IdxVec<HelperId, HelperEntry>,
    pub(in crate::engine) capture_locs: super::CaptureLocations,
    pub(crate) inst_frame_locals: IdxVec<InstanceId, Vec<Ty>>,
    pub(crate) inst_sites: IdxVec<InstanceId, Vec<SiteEntry>>,
    pub(crate) live_instances: BitSet,
    pub(crate) escaping_env_templates: BitSet,

    pub(crate) boxed_locals: BitSet,

    pub(crate) class_static_field_tys: IdxVec<ClassId, Vec<Ty>>,
    pub(crate) class_inst_members: IdxVec<ClassInstId, ClassInstMembers>,
    pub(crate) class_static_methods: IdxVec<ClassId, Vec<InstanceId>>,
    pub(crate) dict_reads: BitSet,
    pub(crate) self_nested_locs: BitSet,
    pub(crate) self_nested_locals: BitSet,
    pub(crate) deletes_array_elements: bool,
}
