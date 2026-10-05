//! WIT layouts describe the complete canonical graph owned by an adapter scope.
use super::*;

const BUFFER: u32 = 1;
const RECORD: u32 = 2;
const VARIANT: u32 = 3;

/// Schema nodes use byte offsets from the schema base; zero means no references.
fn shape(resolve: &wit_parser::Resolve, sizes: &SizeAlign, ty: Type, words: &mut Vec<u32>) -> u32 {
    let record = |fields: Vec<Type>, words: &mut Vec<u32>| {
        let mut entries = Vec::new();
        for (offset, ty) in sizes.field_offsets(&fields) {
            let child = shape(resolve, sizes, *ty, words);
            if child != 0 {
                entries.extend([offset.size_wasm32() as u32, child]);
            }
        }
        if entries.is_empty() {
            return Vec::new();
        }
        let mut node = vec![RECORD, (entries.len() / 2) as u32];
        node.extend(entries);
        node
    };
    let variant = |tag: Int, cases: Vec<Option<Type>>, words: &mut Vec<u32>| {
        let children: Vec<_> = cases
            .iter()
            .map(|ty| ty.map_or(0, |ty| shape(resolve, sizes, ty, words)))
            .collect();
        if children.iter().all(|child| *child == 0) {
            return Vec::new();
        }
        let width = match tag {
            Int::U8 => 1,
            Int::U16 => 2,
            Int::U32 => 4,
            Int::U64 => unreachable!(),
        };
        let offset = sizes
            .payload_offset(tag, cases.iter().map(Option::as_ref))
            .size_wasm32() as u32;
        let mut node = vec![VARIANT, width, offset, children.len() as u32];
        node.extend(children);
        node
    };
    let node = match ty {
        Type::String => vec![BUFFER, 0, 0],
        Type::Id(id) => match &resolve.types[id].kind {
            TypeDefKind::Type(inner) => return shape(resolve, sizes, *inner, words),
            TypeDefKind::List(inner) => {
                let child = shape(resolve, sizes, *inner, words);
                vec![BUFFER, sizes.size(inner).size_wasm32() as u32, child]
            }
            TypeDefKind::Record(fields) => {
                record(fields.fields.iter().map(|field| field.ty).collect(), words)
            }
            TypeDefKind::Tuple(fields) => record(fields.types.clone(), words),
            TypeDefKind::Option(inner) => variant(Int::U8, vec![None, Some(*inner)], words),
            TypeDefKind::Result(result) => variant(Int::U8, vec![result.ok, result.err], words),
            TypeDefKind::Variant(cases) => variant(
                cases.tag(),
                cases.cases.iter().map(|case| case.ty).collect(),
                words,
            ),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    if node.is_empty() {
        return 0;
    }
    let index = (words.len() * 4) as u32;
    words.extend(node);
    index
}

impl Adapter<'_> {
    pub(super) fn allocate_canonical_result(&mut self, ty: Type) -> Value {
        let pointer = self.allocate(
            (self.sizes.size(&ty).size_wasm32() as u32).max(1),
            self.sizes.align(&ty).align_wasm32() as u32,
        );
        let size = self.integer(self.sizes.size(&ty).size_wasm32() as u32);
        let zero = self.integer(0);
        self.body.add_op(
            self.block,
            Operator::MemoryFill {
                mem: self.registry.memory,
            },
            &[pointer, zero, size],
            &[],
        );
        self.retain_canonical_value(ty, pointer, 0);
        pointer
    }

    pub(super) fn retain_canonical_value(&mut self, ty: Type, pointer: Value, offset: u32) {
        let mut words = vec![0];
        let root = shape(&self.wit.resolve, &self.sizes, ty, &mut words);
        if root == 0 {
            return;
        }
        let offset = self.integer(offset);
        let pointer = self.op(Operator::I32Add, &[pointer, offset], CoreType::I32);
        let schema = self.allocate((words.len() * 4) as u32, 4);
        for (index, word) in words.into_iter().enumerate() {
            let word = self.integer(word);
            self.store_i32(schema, (index * 4) as u32, word);
        }
        let owner = self.allocate(12, 4);
        let index = self.integer(root);
        for (offset, value) in [(0, pointer), (4, schema), (8, index)] {
            self.store_i32(owner, offset, value);
        }
        crate::waffle_backend::allocation::tag_allocation(
            &mut self.body,
            self.block,
            self.registry.memory,
            owner,
            crate::waffle_backend::allocation::AllocationKind::CanonicalValue,
        );
    }
}
