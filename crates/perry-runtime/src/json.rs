//! Task-owned JSON variant trees. Arrays and objects retain identity through every lookup.
use crate::shape::{Kind as NativeKind, Record, Type};
use crate::{array, exception, region::perry_alloc, string};
use core::ptr;
use perry_json_codec::{Error, Graph, Tree, Value};
mod native;
mod object;
mod properties;
use object::{Field, Object};
pub use properties::*;
pub mod view;
pub use native::perry_json_pack;

#[derive(Clone, Copy)]
enum Json {
    Null,
    Boolean(bool),
    Number(f64),
    String(*mut u8),
    Array(*mut u8),
    Object(*mut Object),
    Native { reference: *mut u8, ty: *const Type },
}
fn allocate(value: Json) -> *mut u8 {
    let result = perry_alloc(size_of::<Json>() as u32).cast::<Json>();
    // SAFETY: fresh task storage has the size and alignment of Json.
    unsafe {
        result.write(value);
    }
    result.cast()
}
fn failure(kind: u32, message: &'static str) -> *mut u8 {
    exception::raise_error(kind, message);
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_json_null() -> *mut u8 {
    allocate(Json::Null)
}
#[unsafe(no_mangle)]
pub extern "C" fn perry_json_number(value: f64) -> *mut u8 {
    allocate(Json::Number(value))
}
#[unsafe(no_mangle)]
pub extern "C" fn perry_json_boolean(value: u32) -> *mut u8 {
    allocate(Json::Boolean(value != 0))
}
/// # Safety
/// `value` is a live task string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_string(value: *mut u8) -> *mut u8 {
    allocate(Json::String(value))
}
/// # Safety
/// `value` is a live dense array of optional JSON references.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_array(value: *mut u8) -> *mut u8 {
    // SAFETY: the argument is a live native JSON array or a checked typed view.
    unsafe { view::source(value).unwrap_or_else(|| allocate(Json::Array(value))) }
}
/// # Safety
/// Keys are live strings and values optional JSON references, in equally sized dense arrays.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_object(keys: *mut u8, values: *mut u8) -> *mut u8 {
    // SAFETY: the caller supplies matching typed arrays; each field is initialized below.
    unsafe {
        let length = view::perry_view_length(keys) as usize;
        assert_eq!(length, view::perry_view_length(values) as usize);
        let object = Object::new(length);
        for index in 0..length {
            let key = if view::source(keys).is_some() {
                view::perry_view_get(keys, index as u32) as usize as *mut u8
            } else {
                array::slots(keys)
                    .as_ptr()
                    .add(index)
                    .cast::<*mut u8>()
                    .read()
            };
            let value = if view::source(values).is_some() {
                view::perry_view_get(values, index as u32) as usize as *mut u8
            } else {
                array::slots(values)
                    .as_ptr()
                    .add(index)
                    .cast::<*mut u8>()
                    .read()
            };
            if exception::perry_exception_status() != 0 {
                return ptr::null_mut();
            }
            (*object).set(key, value);
        }
        allocate(Json::Object(object))
    }
}

fn from_graph(graph: &Graph<'_>, node: u32) -> Result<*mut u8, Error> {
    Ok(match graph.value(node)? {
        Value::Undefined => return Err(Error::InvalidGraph),
        Value::Null => perry_json_null(),
        Value::Boolean(value) => perry_json_boolean(u32::from(value)),
        Value::Number(value) => perry_json_number(value),
        Value::String(value) => {
            // SAFETY: the parsed graph and these UTF-8 bytes live for the task.
            unsafe {
                perry_json_string(string::perry_string_adopt(
                    value.as_ptr().cast_mut(),
                    value.len() as u32,
                ))
            }
        }
        Value::Array(children) => {
            let array = array::perry_array_new(children.clone().count() as u32);
            for (index, child) in children.enumerate() {
                let value = from_graph(graph, child?)?;
                // SAFETY: appending one initialized JSON reference at a time.
                unsafe {
                    array::set_slot(array, index, value as usize as u64);
                }
            }
            allocate(Json::Array(array))
        }
        Value::Object(children) => {
            let length = children.clone().count();
            let object = Object::new(length);
            for child in children {
                let child = child?;
                let key = graph.key(child)?;
                let value = from_graph(graph, child)?;
                // SAFETY: each field is fresh; graph key bytes remain live for the task.
                unsafe {
                    let key = string::perry_string_adopt(key.as_ptr().cast_mut(), key.len() as u32);
                    (*object).set(key, value);
                }
            }
            allocate(Json::Object(object))
        }
    })
}

/// # Safety
/// `input` is a live runtime string. A null result has a pending exception.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_parse(input: *mut u8) -> *mut u8 {
    // SAFETY: input is live and parsing does not mutate it.
    let text = unsafe { string::text(input) };
    let size = match perry_json_codec::measure(text) {
        Ok(size) => size,
        Err(Error::Depth(_)) => {
            return failure(exception::RANGE_ERROR, "JSON nesting limit exceeded");
        }
        Err(Error::UnpairedSurrogate(_)) => {
            return failure(
                exception::TYPE_ERROR,
                "JSON string contains an unpaired surrogate",
            );
        }
        Err(Error::Capacity) => {
            return failure(exception::RANGE_ERROR, "JSON document is too large");
        }
        Err(Error::Syntax(_)) => return failure(exception::SYNTAX_ERROR, "invalid JSON text"),
        Err(Error::InvalidGraph) => unreachable!("measuring text does not read a graph"),
    };
    let bytes = perry_alloc(size.try_into().expect("JSON document too large"));
    // SAFETY: fresh storage holds the measured graph; borrowed strings keep its task lifetime.
    let bytes = unsafe { core::slice::from_raw_parts_mut(bytes, size) };
    let root = perry_json_codec::populate(text, bytes, 8).expect("measured JSON graph");
    from_graph(&Graph { bytes, base: 8 }, root).expect("validated JSON graph")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Node {
    Value(*mut u8),
    Field(*const Field),
    NativeSlot(native::Slot),
}
#[derive(Clone)]
enum Children {
    Array(core::slice::Iter<'static, u64>),
    Object(core::slice::Iter<'static, Field>),
    NativeArray {
        next: *const u8,
        remaining: usize,
        ty: *const Type,
    },
    NativeObject {
        reference: *mut u8,
        next: usize,
        ty: *const Type,
    },
}
impl Iterator for Children {
    type Item = Result<Node, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Array(values) => values
                .next()
                .map(|&value| Ok(Node::Value(value as usize as *mut u8))),
            Self::Object(fields) => fields
                .find(|field| !field.key.is_null())
                .map(|field| Ok(Node::Field(field))),
            Self::NativeArray {
                next,
                remaining,
                ty,
            } => {
                if *remaining == 0 {
                    return None;
                }
                let slot = native::Slot {
                    address: next.cast_mut(),
                    ty: *ty,
                    key: ptr::null_mut(),
                };
                // SAFETY: this iterator was created over an immutable initialized array span.
                *next = unsafe { next.add(8) };
                *remaining -= 1;
                Some(Ok(Node::NativeSlot(slot)))
            }
            Self::NativeObject {
                reference,
                next,
                ty,
            } => {
                // SAFETY: the descriptor, property order and record remain live and unchanged.
                unsafe {
                    if *next == (**ty).count as usize {
                        return None;
                    }
                    let index = (*reference.cast::<Record>()).order.add(*next).read() as usize;
                    *next += 1;
                    let field = &(**ty).fields()[index];
                    Some(Ok(Node::NativeSlot(native::Slot {
                        address: Record::slot(*reference, index),
                        ty: field.ty,
                        key: field.name,
                    })))
                }
            }
        }
    }
}
struct RuntimeTree;
impl Tree for RuntimeTree {
    type Node = Node;
    type Children = Children;
    fn value(&self, node: Node) -> Result<Value<'_, Children>, Error> {
        // SAFETY: this private adapter is only used with live task JSON nodes; serialization is read-only.
        unsafe {
            let value = match node {
                Node::Value(value) => (!value.is_null()).then(|| *value.cast::<Json>()),
                Node::Field(field) => {
                    (!(*field).value.is_null()).then(|| *(*field).value.cast::<Json>())
                }
                Node::NativeSlot(slot) => native::read(&*slot.ty, (*slot.ty).read(slot.address)),
            };
            let Some(value) = value else {
                return Ok(Value::Undefined);
            };
            Ok(match value {
                Json::Null => Value::Null,
                Json::Boolean(value) => Value::Boolean(value),
                Json::Number(value) => Value::Number(value),
                Json::String(value) => Value::String(string::text(value)),
                Json::Array(value) => Value::Array(Children::Array(array::slots(value).iter())),
                Json::Object(object) => Value::Object(Children::Object((*object).fields().iter())),
                Json::Native { reference, ty } => {
                    let children = native::children(reference, &*ty);
                    if (*ty).kind == NativeKind::Array {
                        Value::Array(children)
                    } else {
                        Value::Object(children)
                    }
                }
            })
        }
    }
    fn key(&self, node: Node) -> Result<&str, Error> {
        let key = match node {
            // SAFETY: object iterators carry initialized fields.
            Node::Field(field) => unsafe { (*field).key },
            Node::NativeSlot(slot) if !slot.key.is_null() => slot.key,
            _ => return Err(Error::InvalidGraph),
        };
        // SAFETY: object members hold live string keys.
        Ok(unsafe { string::text(key) })
    }
}

/// # Safety
/// `value` is a live JSON tree, which remains unchanged during serialization.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_stringify(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return ptr::null_mut();
    }
    let node = Node::Value(value);
    let size = match perry_json_codec::measure_tree(&RuntimeTree, node) {
        Ok(size) => size,
        Err(_) => {
            return failure(
                exception::TYPE_ERROR,
                "JSON value is cyclic or too deeply nested",
            );
        }
    };
    let data = perry_alloc(size.try_into().expect("JSON text too large"));
    // SAFETY: measured output is fresh and disjoint from the read-only JSON tree.
    unsafe {
        let output = core::slice::from_raw_parts_mut(data, size);
        perry_json_codec::serialize_tree(&RuntimeTree, node, output)
            .expect("measured JSON serialization");
        string::perry_string_adopt(data, size as u32)
    }
}

fn canonical_index(key: &str) -> Option<usize> {
    if key.is_empty()
        || !key.bytes().all(|byte| byte.is_ascii_digit())
        || (key.len() > 1 && key.starts_with('0'))
    {
        return None;
    }
    key.parse().ok()
}

/// # Safety
/// `value` is a live JSON node; `key` is a live runtime string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_get(value: *mut u8, key: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller; fields and array storage belong to the same task.
    unsafe {
        match *value.cast::<Json>() {
            Json::Native { reference, ty } => native::get(reference, &*ty, string::text(key)),
            Json::Null => failure(exception::TYPE_ERROR, "cannot read a property of null"),
            Json::Object(object) => (*object)
                .find(string::text(key))
                .map_or(ptr::null_mut(), |field| (*field).value),
            Json::Array(array) if string::text(key) == "length" => {
                perry_json_number(array::slots(array).len() as f64)
            }
            Json::Array(array) => {
                let key = string::text(key);
                let slots = array::slots(array);
                canonical_index(key)
                    .and_then(|index| slots.get(index))
                    .map_or(ptr::null_mut(), |&slot| slot as usize as *mut u8)
            }
            Json::String(text) if string::text(key) == "length" => {
                perry_json_number((*text.cast::<string::Str>()).utf16 as f64)
            }
            Json::String(_) if canonical_index(string::text(key)).is_some() => failure(
                exception::TYPE_ERROR,
                "JSON string indexing is not implemented yet",
            ),
            _ => ptr::null_mut(),
        }
    }
}

/// # Safety
/// `value` is a live JSON node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_index(value: *mut u8, index: f64) -> *mut u8 {
    // SAFETY: guaranteed by the caller; reads only initialized in-range slots.
    unsafe {
        match *value.cast::<Json>() {
            Json::Native { reference, ty } => native::index(reference, &*ty, index),
            Json::Null => failure(exception::TYPE_ERROR, "cannot index null"),
            Json::Object(object) => {
                let mut buffer = ryu_js::Buffer::new();
                (*object)
                    .find(buffer.format(index))
                    .map_or(ptr::null_mut(), |field| (*field).value)
            }
            Json::Array(array) => {
                let slots = array::slots(array);
                if index >= 0.0 && index < slots.len() as f64 && libm::trunc(index) == index {
                    slots[index as usize] as usize as *mut u8
                } else {
                    ptr::null_mut()
                }
            }
            Json::String(_) => failure(
                exception::TYPE_ERROR,
                "JSON string indexing is not implemented yet",
            ),
            Json::Boolean(_) | Json::Number(_) => ptr::null_mut(),
        }
    }
}

/// # Safety
/// `value` is null (undefined) or a live JSON node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_as_number(value: *mut u8) -> f64 {
    // SAFETY: the pointer is checked before dereferencing and the variant before reading its payload.
    if !value.is_null()
        && let Json::Number(number) = unsafe { *value.cast::<Json>() }
    {
        return number;
    }
    failure(exception::TYPE_ERROR, "JSON value is not a number");
    f64::NAN
}
/// # Safety
/// `value` is null (undefined) or a live JSON node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_as_boolean(value: *mut u8) -> u32 {
    // SAFETY: the pointer is checked before dereferencing and the variant before reading its payload.
    if !value.is_null()
        && let Json::Boolean(boolean) = unsafe { *value.cast::<Json>() }
    {
        return boolean as u32;
    }
    failure(exception::TYPE_ERROR, "JSON value is not a boolean");
    0
}
/// # Safety
/// `value` is null (undefined) or a live JSON node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_as_string(value: *mut u8) -> *mut u8 {
    // SAFETY: the pointer is checked before dereferencing and the variant before reading its payload.
    if !value.is_null()
        && let Json::String(string) = unsafe { *value.cast::<Json>() }
    {
        return string;
    }
    failure(exception::TYPE_ERROR, "JSON value is not a string")
}
/// # Safety
/// `value` is null (undefined) or a live JSON node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_typeof(value: *mut u8) -> *mut u8 {
    let name = if value.is_null() {
        "undefined"
    } else {
        // SAFETY: the non-null pointer is live.
        match unsafe { *value.cast::<Json>() } {
            Json::Null | Json::Array(_) | Json::Object(_) | Json::Native { .. } => "object",
            Json::Boolean(_) => "boolean",
            Json::Number(_) => "number",
            Json::String(_) => "string",
        }
    };
    // SAFETY: the literal is valid UTF-8 with static lifetime.
    unsafe { string::perry_string_adopt(name.as_ptr().cast_mut(), name.len() as u32) }
}
/// # Safety
/// Both operands are null (undefined) or live JSON nodes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_equal(a: *mut u8, b: *mut u8) -> u32 {
    if a.is_null() || b.is_null() {
        return (a == b) as u32;
    }
    // SAFETY: non-null operands are live. Composite values compare by storage identity.
    unsafe {
        match (*a.cast::<Json>(), *b.cast::<Json>()) {
            (Json::Null, Json::Null) => 1,
            (Json::Boolean(a), Json::Boolean(b)) => (a == b) as u32,
            (Json::Number(a), Json::Number(b)) => (a == b) as u32,
            (Json::String(a), Json::String(b)) => string::perry_string_equal(a, b),
            (Json::Array(a), Json::Array(b)) => (a == b) as u32,
            (Json::Object(a), Json::Object(b)) => (a == b) as u32,
            (Json::Native { reference: a, .. }, Json::Native { reference: b, .. }) => {
                (a == b) as u32
            }
            _ => 0,
        }
    }
}

/// # Safety
/// `value` is null (undefined) or a live JSON node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_is_array(value: *mut u8) -> u32 {
    // SAFETY: non-null pointers are live.
    if value.is_null() {
        return 0;
    }
    // SAFETY: a live JSON value's native descriptor is static.
    unsafe {
        match *value.cast::<Json>() {
            Json::Array(_) => 1,
            Json::Native { ty, .. } => ((*ty).kind == NativeKind::Array) as u32,
            _ => 0,
        }
    }
}
/// # Safety
/// Value and key are live JSON/string references; replacement is undefined or a live JSON value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_set(value: *mut u8, key: *mut u8, replacement: *mut u8) {
    // SAFETY: references are live; object headers retain identity across growth.
    unsafe {
        if let Json::Native { reference, ty } = *value.cast::<Json>() {
            native::set_key(reference, &*ty, string::text(key), replacement);
            return;
        }
        if matches!(*value.cast::<Json>(), Json::Array(_))
            && let Some(index) = canonical_index(string::text(key))
        {
            perry_json_set_index(value, index as f64, replacement);
            return;
        }
        if let Json::Object(object) = *value.cast::<Json>() {
            if string::text(key) == "__proto__" && (*object).find("__proto__").is_none() {
                failure(
                    exception::TYPE_ERROR,
                    "prototype setters are outside the closed-shape subset",
                );
                return;
            }
            (*object).set(key, replacement);
            return;
        }
    }
    failure(
        exception::TYPE_ERROR,
        "JSON property assignment requires an object or dense array index",
    );
}
/// # Safety
/// Value is a live JSON value; replacement is undefined or a live JSON value in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_set_index(value: *mut u8, index: f64, replacement: *mut u8) {
    // SAFETY: references are live; dense bounds are validated before mutation.
    unsafe {
        if let Json::Native { reference, ty } = *value.cast::<Json>() {
            if (*ty).kind == NativeKind::Array {
                native::set_index(reference, &*ty, index, replacement);
            } else {
                let mut buffer = ryu_js::Buffer::new();
                native::set_key(reference, &*ty, buffer.format(index), replacement);
            }
            return;
        }
        if let Json::Object(object) = *value.cast::<Json>() {
            let mut buffer = ryu_js::Buffer::new();
            let text = buffer.format(index);
            if let Some(field) = (*object).find(text) {
                (*field).value = replacement;
            } else {
                let key = string::copy(text);
                (*object).set(key, replacement);
            }
            return;
        }
        if let Json::Array(array) = *value.cast::<Json>() {
            let length = array::slots(array).len();
            if index >= 0.0 && index <= length as f64 && libm::trunc(index) == index {
                array::set_slot(array, index as usize, replacement as usize as u64);
                return;
            }
            failure(
                exception::RANGE_ERROR,
                "JSON array write would leave a hole",
            );
            return;
        }
    }
    failure(
        exception::TYPE_ERROR,
        "JSON indexed assignment requires an array",
    );
}

/// # Safety
/// `value` is null (undefined) or a live JSON node. The returned array aliases its storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_as_array(value: *mut u8) -> *mut u8 {
    const JSON: Type = Type {
        kind: NativeKind::Json,
        detail: ptr::null(),
        count: 0,
    };
    const ARRAY: Type = Type {
        kind: NativeKind::Array,
        detail: (&JSON as *const Type).cast(),
        count: 0,
    };
    // SAFETY: the static descriptor matches JsonValue[]; conversion validates the source.
    unsafe { view::perry_json_unpack(&ARRAY, value) as usize as *mut u8 }
}
