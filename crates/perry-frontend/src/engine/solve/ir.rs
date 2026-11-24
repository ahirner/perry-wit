//! Solved inference tables, call-site resolutions, and synthesized helper specs.
use super::super::bitset::BitSet;
use super::super::types::{
    CaptureOwner, ClassId, ClassInstId, FuncIdentity, HelperId, IdxVec, InstanceId, LocId,
    LocalIdx, TemplateId, Ty, TySpan,
};
use perry_hir::{ir::CompareOp, types::LocalId};
use perry_wit_diagnostics::{Diagnostic, Result};

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

pub(crate) struct Instance {
    pub(crate) is_witness: bool,
    pub(crate) list_result: Option<Ty>,
    pub(crate) template: TemplateId,
    pub(crate) receiver: Option<Receiver>,
    pub(crate) capture_owner: CaptureOwner,
    pub(crate) args: Ty,
    pub(crate) explicit_type_args: Ty,
    pub(crate) return_loc: LocId,
    pub(crate) has_explicit_return: bool,
    pub(crate) param_locs: TySpan,
    pub(crate) caller: Option<InstanceId>,
    pub(crate) call_byte_offset: u32,
    pub(crate) is_inline_callback: bool,
    pub(crate) captures_non_global: bool,
    pub(crate) emitted_func_id: u32,
    pub(crate) emitted_name: String,
    pub(crate) emitted_method_name: String,
}

pub(crate) struct ClassInst {
    pub(crate) class: ClassId,
    pub(crate) args: Ty,
    pub(crate) field_locs: TySpan,
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
pub(crate) enum CallbackCall {
    Direct {
        target: InstanceId,
        pass_env: bool,
        pass_receiver: bool,
    },
    Dispatch {
        helper: HelperId,
        needs_recv: bool,
    },
    ThrowTypeError(HelperId),
}

impl CallbackCall {
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
        callee_ty: Ty,
        arms: TySpan,
        arg_tys: Ty,
        recv_ty: Option<Ty>,
    },
    Widener {
        from: Ty,
        to: Ty,
    },
    Comparer {
        left_ty: Ty,
        right_ty: Ty,
    },
    TypeOfFn(Ty),
    ThrowTypeError,
    PromiseReaction {
        kind: ReactionKind,
        promise_ty: Ty,
        inner_ty: Ty,
        on_fulfilled: Option<(CallbackCall, Ty)>,
        on_rejected: Option<(CallbackCall, Ty)>,
        ret_ty: Ty,
    },
    TimerRunner {
        repeating: bool,
        is_immediate: bool,
        cb_call: CallbackCall,
        cb_ty: Ty,
        extra_arg_tys: Ty,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FuncCoercion {
    Zero,
    Tag(FuncIdentity),
    BoxEnv(FuncIdentity),
    Widen(HelperId),
    DispatchCallback {
        helper: HelperId,
        arms: TySpan,
        input_tys: Ty,
    },
    EnvCallback {
        target: InstanceId,
        input_tys: Ty,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum SiteResolution {
    DirectCall {
        target: InstanceId,
        argument_types: Ty,
        pass_receiver: bool,
        pass_env: bool,
    },
    DispatchCall {
        helper: HelperId,
        argument_types: Ty,
        needs_recv: bool,
    },
    ThrowTypeError(HelperId),
    FuncTypeOf(Option<HelperId>),
    FuncCompare {
        left_ty: Ty,
        right_ty: Ty,
        op: CompareOp,
        helper: Option<HelperId>,
    },
    MethodCall {
        target: InstanceId,
        argument_types: Ty,
    },
    Callback {
        target: InstanceId,
        keep_inline_closure: bool,
    },
    ClassNew {
        class_inst: ClassInstId,
        argument_types: Ty,
    },
    ResourceCall(usize),
    BranchResult(Ty),
    ObjectLiteral(Ty),
    ZeroSizedArray,
    TypedArray(Ty),
    ZeroSizedFieldSet,
    CollectionNew {
        is_map: bool,
        type_args: Vec<Ty>,
    },
    Cell(LocId),
    FoldBool(bool),
    PromiseNew {
        value_loc: LocId,
        resolve_tid: TemplateId,
        reject_tid: TemplateId,
        executor: Option<(CallbackCall, bool, bool)>,
    },
    PromiseReaction {
        result: LocId,
        helper: HelperId,
        pass_first: bool,
        pass_second: bool,
    },
    TimerStart {
        helper: HelperId,
        has_delay: bool,
        is_immediate: bool,
        pass_cb: bool,
    },
    TimerClear {
        arg_ty: Option<Ty>,
    },
}

#[derive(Clone, Default, Debug)]
pub(crate) struct SiteEntry {
    pub(crate) resolution: Option<SiteResolution>,
    pub(crate) coercion: Option<FuncCoercion>,
}

impl SiteEntry {
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

pub(crate) struct Solved {
    pub(crate) witness_instances: Vec<Option<InstanceId>>,
    pub(crate) init_inst: InstanceId,
    pub(crate) instances: IdxVec<InstanceId, Instance>,
    pub(crate) class_insts: IdxVec<ClassInstId, ClassInst>,
    pub(crate) locations: IdxVec<LocId, Ty>,
    pub(crate) param_locs: Vec<LocId>,
    pub(crate) field_locs: Vec<LocId>,
    pub(crate) dispatch_arms: Vec<(FuncIdentity, InstanceId)>,
    pub(crate) helpers: IdxVec<HelperId, HelperSpec>,
    pub(crate) helper_byte_offsets: IdxVec<HelperId, u32>,
    pub(crate) inst_local_tys: IdxVec<InstanceId, Vec<(LocalId, Ty)>>,
    pub(crate) inst_frame_locals: IdxVec<InstanceId, Vec<Ty>>,
    pub(crate) inst_sites: IdxVec<InstanceId, Vec<SiteEntry>>,
    pub(crate) inst_captures: IdxVec<InstanceId, Vec<LocalId>>,
    pub(crate) live_instances: BitSet,
    pub(crate) escaping_env_templates: BitSet,
    pub(crate) collapsed_closures: BitSet,
    pub(crate) boxed_locals: BitSet,
    pub(crate) module_local_tys: IdxVec<LocalIdx, Option<Ty>>,
    pub(crate) collapsed_local_tys: IdxVec<LocalIdx, Option<Ty>>,
    pub(crate) class_static_field_tys: IdxVec<ClassId, Vec<Ty>>,
    pub(crate) class_inst_methods: IdxVec<ClassInstId, Vec<InstanceId>>,
    pub(crate) class_inst_getters: IdxVec<ClassInstId, Vec<(usize, InstanceId)>>,
    pub(crate) class_inst_setters: IdxVec<ClassInstId, Vec<(usize, InstanceId)>>,
    pub(crate) class_static_methods: IdxVec<ClassId, Vec<InstanceId>>,
    pub(crate) dict_reads: BitSet,
    pub(crate) self_nested_locs: BitSet,
    pub(crate) self_nested_locals: BitSet,
    pub(crate) deletes_array_elements: bool,
}
