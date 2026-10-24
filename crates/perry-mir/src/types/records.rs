use super::{Context, Identifier, RecordKind, RecordType, Result, TypeHandle, TypedHandle};
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

type CarrierLayoutKey = (RecordKind, Vec<(String, TypeHandle)>);

#[derive(Default)]
struct RecordLayouts {
    records: Mutex<HashMap<CarrierLayoutKey, TypedHandle<RecordType>>>,
    next_record: AtomicUsize,
}

pliron::dict_key!(RECORD_LAYOUTS, "perry_mir_record_layouts");
pliron::context_registration!(register_record_layouts);

fn register_record_layouts(ctx: &mut Context) {
    let index = ctx.aux_data.insert(Box::<RecordLayouts>::default());
    ctx.aux_data_map.insert(RECORD_LAYOUTS.clone(), index);
}

fn layouts(ctx: &Context) -> &RecordLayouts {
    let index = ctx.aux_data_map[&RECORD_LAYOUTS];
    ctx.aux_data[index]
        .downcast_ref::<RecordLayouts>()
        .expect("record layouts registered on Context")
}

impl RecordType {
    pub fn fresh(
        ctx: &Context,
        kind: RecordKind,
        fields: Option<Vec<(String, TypeHandle)>>,
    ) -> Result<TypedHandle<Self>> {
        loop {
            let id = layouts(ctx).next_record.fetch_add(1, Ordering::Relaxed) + 1;
            let name = Identifier::try_from(format!("shape_{id}"))
                .expect("generated record name is a valid identifier");
            let Ok(record) = Self::get_kind(ctx, name.clone(), None, kind) else {
                continue;
            };
            if record.deref(ctx).fields().is_ok() {
                continue;
            }
            return match fields {
                Some(fields) => Self::get_kind(ctx, name, Some(fields), kind),
                None => Ok(record),
            };
        }
    }

    /// Reuse a carrier layout; distinct source identities may have the same representation.
    pub fn structural(
        ctx: &Context,
        kind: RecordKind,
        mut fields: Vec<(String, TypeHandle)>,
    ) -> Result<TypedHandle<Self>> {
        if kind == RecordKind::Object {
            fields.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        }
        let key = (kind, fields);
        let layouts = layouts(ctx);
        if let Some(record) = layouts
            .records
            .lock()
            .expect("record layouts lock poisoned")
            .get(&key)
        {
            return Ok(*record);
        }
        let record = Self::fresh(ctx, kind, Some(key.1.clone()))?;
        layouts
            .records
            .lock()
            .expect("record layouts lock poisoned")
            .insert(key, record);
        Ok(record)
    }
}
