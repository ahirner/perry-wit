//! Standard byte-stream objects retain the underlying body and serialize demand.
use crate::waffle_backend::{
    allocation::AllocationFuncs,
    bytes::ByteHelpers,
    objects::ObjectHelpers,
    registry::PromiseImports,
    runtime::{
        builder::{self, Builder},
        operations::Operations,
    },
    strings::StringPool,
};
use anyhow::Result;
use perry_hir::types::{ObjectType, PropertyInfo, Type as HirType};
use waffle::{
    Func, Memory, Module, Operator as O,
    Type::{F64, I32},
    Value,
};

pub(super) mod byob;
mod read;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Readable,
    Reader,
    ByobReader,
    Writable,
    Writer,
}
impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Readable => "__perry_readable_bytes",
            Self::Reader => "__perry_byte_reader",
            Self::ByobReader => "__perry_byob_reader",
            Self::Writable => "__perry_writable_bytes",
            Self::Writer => "__perry_byte_writer",
        }
    }
    pub(crate) fn ty(self) -> HirType {
        HirType::Named(self.name().into())
    }
    pub(crate) fn of(ty: &HirType) -> Option<Self> {
        [
            Self::Readable,
            Self::Reader,
            Self::ByobReader,
            Self::Writable,
            Self::Writer,
        ]
        .into_iter()
        .find(|kind| kind.ty() == *ty)
    }
}
pub(crate) fn body_type() -> HirType {
    HirType::Union(vec![Kind::Readable.ty(), HirType::Null])
}
pub(crate) fn read_type() -> HirType {
    HirType::Object(ObjectType {
        properties: [
            (
                "done".into(),
                PropertyInfo {
                    ty: HirType::Boolean,
                    optional: false,
                    readonly: false,
                },
            ),
            (
                "value".into(),
                PropertyInfo {
                    ty: HirType::Named("Uint8Array".into()),
                    optional: true,
                    readonly: false,
                },
            ),
        ]
        .into(),
        property_order: Some(vec!["done".into(), "value".into()]),
        ..Default::default()
    })
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Method {
    Read,
    ReadInto,
    Cancel,
    Write,
    Close,
}
impl Method {
    pub(crate) fn named(name: &str) -> Option<Self> {
        match name {
            "read" => Some(Self::Read),
            "cancel" => Some(Self::Cancel),
            "write" => Some(Self::Write),
            "close" => Some(Self::Close),
            _ => None,
        }
    }
    pub(crate) fn result(self) -> HirType {
        match self {
            Self::Read | Self::ReadInto => read_type(),
            Self::Cancel | Self::Write | Self::Close => HirType::Void,
        }
    }
}

pub(crate) const STATE: u32 = 0;
pub(crate) const BODY: u32 = 4;
pub(crate) const LOCK: u32 = 8;
pub(crate) const BODY_KIND: u32 = 12;
pub(crate) const TRANSFER: u32 = 16;
pub(crate) const LAST_READ: u32 = 20;
pub(crate) const ERROR: u32 = 24;
pub(crate) const EOF: u32 = 28;
pub(crate) const PENDING_CHUNK: u32 = 32;
const SIZE: u32 = 36;
const CLOSED: u32 = 1;
const ERRORED: u32 = 2;
pub(super) const CANCELLING: u32 = 3;

#[derive(Clone, Copy)]
pub(crate) struct Helpers {
    pub(crate) body: Func,
    pub(crate) input: Func,
    pub(crate) writable: Func,
    pub(crate) write: Func,
    pub(crate) close: Func,
    pub(crate) dispose: Func,
    pub(crate) reader: Func,
    pub(crate) release: Func,
    pub(crate) read: Func,
    pub(crate) read_into: Func,
    pub(crate) cancel: Func,
    pub(crate) pull: Func,
}
impl Helpers {
    pub(crate) fn method(self, method: Method) -> Func {
        match method {
            Method::Read => self.read,
            Method::ReadInto => self.read_into,
            Method::Cancel => self.cancel,
            Method::Write => self.write,
            Method::Close => self.close,
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) struct NativeBody {
    pub(crate) read: Func,
    pub(crate) release: Func,
    pub(crate) operations: Operations,
}
pub(crate) enum ReaderModes {
    Default,
    Byob,
    Both,
}
impl ReaderModes {
    pub(crate) fn for_plan(plan: Option<&crate::waffle_backend::promises::PromisePlan>) -> Self {
        use crate::waffle_backend::promises::TaskTarget;
        let Some(plan) = plan else {
            return Self::Default;
        };
        if !plan
            .tasks
            .contains_key(&TaskTarget::WebStream(Method::ReadInto))
        {
            return Self::Default;
        }
        if plan.tasks.keys().any(|target| {
            matches!(
                target,
                TaskTarget::HttpBody(_) | TaskTarget::WebStream(Method::Read)
            )
        }) {
            Self::Both
        } else {
            Self::Byob
        }
    }
    pub(crate) fn has_default(&self) -> bool {
        !matches!(self, Self::Byob)
    }
    pub(crate) fn has_byob(&self) -> bool {
        !matches!(self, Self::Default)
    }
}
pub(crate) struct Runtime<'a> {
    pub(crate) readers: ReaderModes,
    pub(crate) allocator: AllocationFuncs,
    pub(crate) bytes: ByteHelpers,
    pub(crate) objects: ObjectHelpers,
    pub(crate) promises: Option<&'a PromiseImports>,
    pub(crate) pool: &'a StringPool,
    pub(crate) native: Option<NativeBody>,
    pub(crate) input: Option<super::incoming::Runtime>,
    pub(crate) output: Option<[Option<Func>; 2]>,
}
fn pointer(b: &mut Builder, base: Value, offset: u32) -> Value {
    let offset = b.integer(offset);
    b.op(O::I32Add, &[base, offset], I32)
}
pub(super) fn reject(b: &mut Builder, condition: Value, code: u32) {
    let bad = b.body.add_block();
    let next = b.body.add_block();
    b.branch(condition, bad, next);
    b.block = bad;
    let one = b.integer(1);
    let error = b.number(f64::from(code));
    b.ret(&[one, error]);
    b.block = next;
}
pub(super) fn tagged_allocation(
    b: &mut Builder,
    a: AllocationFuncs,
    size: u32,
    kind: u32,
) -> Value {
    let value = b.allocate(a.realloc, size, 4);
    let zero = b.integer(0);
    let size = b.integer(size);
    b.effect(
        O::MemoryFill {
            mem: b.memory(0).memory,
        },
        &[value, zero, size],
    );
    let four = b.integer(4);
    let header = b.op(O::I32Sub, &[value, four], I32);
    let header = b.load(header, 0, I32);
    let kind = b.integer(kind);
    b.store(header, 16, kind, I32);
    value
}
fn used_address(b: &mut Builder, stream: Value) -> Value {
    let body = b.load(stream, BODY, I32);
    let kind = b.load(stream, BODY_KIND, I32);
    let request = b.integer(crate::waffle_backend::http::request::BODY_USED);
    let response = b.integer(20);
    let offset = b.op(O::Select, &[response, request, kind], I32);
    let address = b.op(O::I32Add, &[body, offset], I32);
    let input_kind = b.integer(super::incoming::KIND);
    let input = b.op(O::I32Eq, &[kind, input_kind], I32);
    let used = pointer(b, stream, super::incoming::USED);
    b.op(O::Select, &[used, address, input], I32)
}
fn is_native(b: &mut Builder, stream: Value) -> Value {
    let kind = b.load(stream, BODY_KIND, I32);
    let response = b.body.add_block();
    let request = b.body.add_block();
    let done = b.body.add_block();
    let native = b.body.add_blockparam(done, I32);
    b.branch(kind, response, request);
    b.block = request;
    let zero = b.integer(0);
    b.jump(done, &[zero]);
    b.block = response;
    let body = b.load(stream, BODY, I32);
    let value = b.load(body, crate::waffle_backend::http::response::NATIVE, I32);
    b.jump(done, &[value]);
    b.block = done;
    native
}
pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    r: &Runtime<'_>,
) -> Result<Helpers> {
    let h = Helpers {
        writable: builder::declare(module, "web.writable", &[F64], &[I32]),
        write: builder::declare(module, "web.write", &[I32; 3], &[I32, F64]),
        close: builder::declare(module, "web.close", &[I32; 3], &[I32, F64]),
        input: builder::declare(module, "web.input", &[I32], &[I32]),
        dispose: builder::declare(module, "web.dispose-input", &[I32], &[]),
        body: builder::declare(module, "web.body", &[I32; 2], &[I32]),
        reader: builder::declare(module, "web.get-reader", &[I32], &[I32, F64]),
        release: builder::declare(module, "web.release-reader", &[I32], &[I32, F64]),
        read: builder::declare(module, "web.read", &[I32; 3], &[I32, F64]),
        read_into: builder::declare(module, "web.read-into", &[I32, I32, F64, I32], &[I32, F64]),
        cancel: builder::declare(module, "web.cancel", &[I32; 3], &[I32, F64]),
        pull: builder::declare(module, "web.pull", &[I32; 2], &[I32; 3]),
    };
    let finish = builder::declare(module, "web.finish", &[I32], &[I32]);
    let result = builder::declare(module, "web.read-result", &[I32; 2], &[I32]);
    emit_body(module, memory, r, h.body)?;
    emit_reader(module, memory, r, h)?;
    read::emit_finish(module, memory, r, finish)?;
    let input_pull = super::incoming::emit(module, memory, r, h, finish)?;
    read::emit_pull(module, memory, r, h.pull, finish, input_pull)?;
    read::emit_result(module, memory, r, result)?;
    let byob_pull = byob::emit(module, memory, h.pull, finish)?;
    read::emit_read(module, memory, r, h.read, h.pull, result, false)?;
    read::emit_read(module, memory, r, h.read_into, byob_pull, result, true)?;
    read::emit_cancel(module, memory, r, h.cancel, finish)?;
    super::writable::emit(module, memory, r, h)?;
    Ok(h)
}
fn emit_body(module: &mut Module<'static>, memory: Memory, r: &Runtime<'_>, f: Func) -> Result<()> {
    use crate::waffle_backend::http::{request, response};
    let mut b = Builder::new(module, f, memory);
    let owner = b.param(0);
    let kind = b.param(1);
    let zero = b.integer(0);
    let req = b.body.add_block();
    let res = b.body.add_block();
    let ready = b.body.add_block();
    let slot = b.body.add_blockparam(ready, I32);
    let present = b.body.add_blockparam(ready, I32);
    b.branch(kind, res, req);
    b.block = req;
    let address = pointer(&mut b, owner, request::STREAM);
    let has = b.load(owner, request::BODY_KIND, I32);
    b.jump(ready, &[address, has]);
    b.block = res;
    let address = pointer(&mut b, owner, response::STREAM);
    let null = b.load(owner, 52, I32);
    let has = b.op(O::I32Eqz, &[null], I32);
    b.jump(ready, &[address, has]);
    b.block = ready;
    let some = b.body.add_block();
    let none = b.body.add_block();
    b.branch(present, some, none);
    b.block = none;
    b.ret(&[zero]);
    b.block = some;
    let cached = b.load(slot, 0, I32);
    let old = b.body.add_block();
    let new = b.body.add_block();
    b.branch(cached, old, new);
    b.block = old;
    b.ret(&[cached]);
    b.block = new;
    let one = b.integer(1);
    let frame = b.call(r.allocator.frame_new, &[one], &[I32])[0];
    b.store(frame, 12, owner, I32);
    let stream = tagged_allocation(&mut b, r.allocator, SIZE, 16);
    b.store(stream, BODY, owner, I32);
    b.store(stream, BODY_KIND, kind, I32);
    b.store(slot, 0, stream, I32);
    let used = used_address(&mut b, stream);
    let used = b.load(used, 0, I32);
    let state = b.op(O::Select, &[one, zero, used], I32);
    b.store(stream, STATE, state, I32);
    b.store(stream, LOCK, used, I32);
    b.call(r.allocator.frame_drop, &[frame], &[]);
    b.ret(&[stream]);
    b.finish(module, f)
}
fn emit_reader(
    module: &mut Module<'static>,
    memory: Memory,
    r: &Runtime<'_>,
    h: Helpers,
) -> Result<()> {
    let mut b = Builder::new(module, h.reader, memory);
    let stream = b.param(0);
    let locked = b.load(stream, LOCK, I32);
    reject(&mut b, locked, 12);
    let one = b.integer(1);
    let frame = b.call(r.allocator.frame_new, &[one], &[I32])[0];
    b.store(frame, 12, stream, I32);
    let reader = tagged_allocation(&mut b, r.allocator, 12, 17);
    b.store(reader, 4, stream, I32);
    b.store(stream, LOCK, reader, I32);
    b.call(r.allocator.frame_drop, &[frame], &[]);
    let zero = b.integer(0);
    let payload = b.op(O::F64ConvertI32U, &[reader], F64);
    b.ret(&[zero, payload]);
    b.finish(module, h.reader)?;
    let mut b = Builder::new(module, h.release, memory);
    let reader = b.param(0);
    let stream = b.load(reader, 4, I32);
    let release = b.body.add_block();
    let done = b.body.add_block();
    b.branch(stream, release, done);
    b.block = release;
    let zero = b.integer(0);
    b.store(stream, LOCK, zero, I32);
    b.store(reader, 4, zero, I32);
    if let Some(promises) = r.promises {
        let head = b.load(reader, 0, I32);
        let next = b.body.add_block();
        let node = b.body.add_blockparam(next, I32);
        let reject = b.body.add_block();
        b.jump(next, &[head]);
        b.block = next;
        b.branch(node, reject, done);
        b.block = reject;
        let record = b.load(node, 8, I32);
        let state = b.load(record, 4, I32);
        let two = b.integer(2);
        let pending = b.op(O::I32Eq, &[state, two], I32);
        let settle = b.body.add_block();
        let advance = b.body.add_block();
        b.branch(pending, settle, advance);
        b.block = settle;
        let one = b.integer(1);
        let error = b.number(12.0);
        b.call(promises.native.settle, &[record, one, error], &[]);
        b.jump(advance, &[]);
        b.block = advance;
        let following = b.load(node, 4, I32);
        b.jump(next, &[following]);
    } else {
        b.jump(done, &[]);
    }
    b.block = done;
    let zero = b.integer(0);
    let payload = b.number(0.0);
    b.ret(&[zero, payload]);
    b.finish(module, h.release)
}
