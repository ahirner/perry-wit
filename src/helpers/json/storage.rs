use super::Error;

/// Graph storage: measuring (`stores` false) only counts the bytes a graph needs; storing
/// writes it to `bytes`, whose first byte has guest address `base`. One type for both keeps
/// a single copy of the parser.
pub(super) struct Storage<'a> {
    pub(super) bytes: &'a mut [u8],
    pub(super) length: usize,
    pub(super) base: u32,
    pub(super) stores: bool,
}

impl Storage<'_> {
    /// Append fully initialized bytes without first clearing their destination.
    pub(super) fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let end = self
            .length
            .checked_add(bytes.len())
            .ok_or(Error::Capacity)?;
        u32::try_from(end).map_err(|_| Error::Capacity)?;
        if self.stores {
            self.bytes
                .get_mut(self.length..end)
                .ok_or(Error::Capacity)?
                .copy_from_slice(bytes);
        }
        self.length = end;
        Ok(())
    }

    pub(super) fn reserve(&mut self, size: usize, alignment: usize) -> Result<usize, Error> {
        let start = self
            .length
            .checked_add(alignment - 1)
            .ok_or(Error::Capacity)?
            & !(alignment - 1);
        let end = start.checked_add(size).ok_or(Error::Capacity)?;
        u32::try_from(end).map_err(|_| Error::Capacity)?;
        if self.stores {
            self.bytes
                .get_mut(self.length..end)
                .ok_or(Error::Capacity)?
                .fill(0);
        }
        self.length = end;
        Ok(start)
    }

    pub(super) fn write(&mut self, position: usize, bytes: &[u8]) -> Result<(), Error> {
        if !self.stores {
            return Ok(());
        }
        let end = position.checked_add(bytes.len()).ok_or(Error::Capacity)?;
        self.bytes
            .get_mut(position..end)
            .ok_or(Error::Capacity)?
            .copy_from_slice(bytes);
        Ok(())
    }

    pub(super) fn address(&self, position: usize) -> Result<u32, Error> {
        self.base
            .checked_add(u32::try_from(position).map_err(|_| Error::Capacity)?)
            .ok_or(Error::Capacity)
    }
}
