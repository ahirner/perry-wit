//! Static type graphs and per-allocation property order, shared with the runtime.
use super::*;
use perry_runtime::shape::{FIELD_SIZE, Kind, TYPE_SIZE};

impl Data {
    fn reserve(&mut self, size: usize) -> u32 {
        self.bytes.resize(self.bytes.len().next_multiple_of(8), 0);
        let address = DATA_BASE + self.bytes.len() as u32;
        self.bytes.resize(self.bytes.len() + size.max(1), 0);
        address
    }
    fn words(&mut self, address: u32, words: &[u32]) {
        let start = (address - DATA_BASE) as usize;
        for (index, word) in words.iter().enumerate() {
            self.bytes[start + 4 * index..start + 4 * index + 4]
                .copy_from_slice(&word.to_le_bytes());
        }
    }
    pub(super) fn property_order(&mut self, order: Vec<usize>) -> u32 {
        if let Some(&address) = self.orders.get(&order) {
            return address;
        }
        let address = self.reserve(4 * order.len());
        self.words(
            address,
            &order.iter().map(|&index| index as u32).collect::<Vec<_>>(),
        );
        self.orders.insert(order, address);
        address
    }
    pub(super) fn descriptor(&mut self, ctx: &Context, ty: TypeHandle) -> Result<u32> {
        if let Some(&address) = self.types.get(&ty) {
            return Ok(address);
        }
        let address = self.reserve(TYPE_SIZE as usize);
        self.types.insert(ty, address);
        let ty = ty.deref(ctx);
        let (kind, detail, count) = if ty.is::<FP64Type>() {
            (Kind::Number, 0, 0)
        } else if ty
            .downcast_ref::<IntegerType>()
            .is_some_and(|ty| ty.width() == 1)
        {
            (Kind::Boolean, 0, 0)
        } else if ty.is::<StringType>() {
            (Kind::String, 0, 0)
        } else if ty.is::<JsonType>() {
            (Kind::Json, 0, 0)
        } else if let Some(optional) = ty.downcast_ref::<OptionalType>() {
            (Kind::Optional, self.descriptor(ctx, optional.value())?, 0)
        } else if let Some(array) = ty.downcast_ref::<ArrayType>() {
            (Kind::Array, self.descriptor(ctx, array.element())?, 0)
        } else if let Some(record) = ty.downcast_ref::<RecordType>() {
            let fields = record
                .fields()
                .map_err(|_| anyhow!("record without fields"))?;
            let detail = self.reserve(fields.len() * FIELD_SIZE as usize);
            for (index, (name, ty)) in fields.iter().enumerate() {
                let name = self.string(name.as_ref());
                let field_type = self.descriptor(ctx, *ty)?;
                self.words(detail + index as u32 * FIELD_SIZE, &[name, field_type]);
            }
            (Kind::Record, detail, fields.len() as u32)
        } else {
            (Kind::Opaque, 0, 0)
        };
        self.words(address, &[kind as u32, detail, count]);
        Ok(address)
    }
}
