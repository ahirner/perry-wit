use super::{Context, Identifier, RecordKind, RecordType, Result, TypeHandle, TypedHandle};
use std::collections::HashMap;

type Fields = Vec<(String, TypeHandle)>;

/// Nominal identities and canonical structural carriers in one compilation.
#[derive(Default)]
pub struct RecordLayouts {
    records: HashMap<(RecordKind, Fields), TypedHandle<RecordType>>,
    next_record: usize,
}

impl RecordLayouts {
    pub fn fresh(
        &mut self,
        ctx: &Context,
        kind: RecordKind,
        fields: Option<Fields>,
    ) -> Result<TypedHandle<RecordType>> {
        loop {
            self.next_record += 1;
            let id = self.next_record;
            let name = Identifier::try_from(format!("shape_{id}"))?;
            let record = RecordType::get_kind(ctx, name.clone(), None, kind)?;
            if record.deref(ctx).fields().is_ok() {
                continue;
            }
            return match fields {
                Some(fields) => RecordType::get_kind(ctx, name, Some(fields), kind),
                None => Ok(record),
            };
        }
    }

    /// Reuse a carrier layout; distinct source identities may have the same representation.
    pub fn structural(
        &mut self,
        ctx: &Context,
        kind: RecordKind,
        mut fields: Fields,
    ) -> Result<TypedHandle<RecordType>> {
        if kind == RecordKind::Object {
            fields.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        }
        let key = (kind, fields);
        if let Some(record) = self.records.get(&key) {
            return Ok(*record);
        }
        let record = self.fresh(ctx, kind, Some(key.1.clone()))?;
        self.records.insert(key, record);
        Ok(record)
    }
}
