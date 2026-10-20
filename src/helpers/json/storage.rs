use super::{Error, Kind, NODE_BYTES, reader::Reader};

/// Caller-owned graph storage. `STORE` selects measurement or writing at compile time,
/// so measuring validates and counts without executing the storage operations.
pub(super) struct Storage<'a, const STORE: bool> {
    pub(super) bytes: &'a mut [u8],
    pub(super) length: usize,
    pub(super) base: u32,
}

/// Storage policy for the shared JSON graph traversal.
pub(super) trait GraphStorage {
    const STORE: bool;
    fn reserve(&mut self, size: usize, alignment: usize) -> Result<usize, Error>;
    fn write(&mut self, position: usize, bytes: &[u8]) -> Result<(), Error>;
    fn address(&self, position: usize) -> Result<u32, Error>;
    fn string(&mut self, reader: &mut Reader<'_>) -> Result<usize, Error>;
}

impl<const STORE: bool> Storage<'_, STORE> {
    /// Append fully initialized bytes without first clearing their destination.
    pub(super) fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let end = self
            .length
            .checked_add(bytes.len())
            .ok_or(Error::Capacity)?;
        u32::try_from(end).map_err(|_| Error::Capacity)?;
        if STORE {
            self.bytes
                .get_mut(self.length..end)
                .ok_or(Error::Capacity)?
                .copy_from_slice(bytes);
        }
        self.length = end;
        Ok(())
    }
}

impl<const STORE: bool> GraphStorage for Storage<'_, STORE> {
    const STORE: bool = STORE;
    fn reserve(&mut self, size: usize, alignment: usize) -> Result<usize, Error> {
        let start = self
            .length
            .checked_add(alignment - 1)
            .ok_or(Error::Capacity)?
            & !(alignment - 1);
        let end = start.checked_add(size).ok_or(Error::Capacity)?;
        u32::try_from(end).map_err(|_| Error::Capacity)?;
        if STORE {
            self.bytes
                .get_mut(self.length..end)
                .ok_or(Error::Capacity)?
                .fill(0);
        }
        self.length = end;
        Ok(start)
    }

    fn write(&mut self, position: usize, bytes: &[u8]) -> Result<(), Error> {
        if !STORE {
            return Ok(());
        }
        let end = position.checked_add(bytes.len()).ok_or(Error::Capacity)?;
        self.bytes
            .get_mut(position..end)
            .ok_or(Error::Capacity)?
            .copy_from_slice(bytes);
        Ok(())
    }

    fn address(&self, position: usize) -> Result<u32, Error> {
        self.base
            .checked_add(u32::try_from(position).map_err(|_| Error::Capacity)?)
            .ok_or(Error::Capacity)
    }
    fn string(&mut self, reader: &mut Reader<'_>) -> Result<usize, Error> {
        let node = self.reserve(NODE_BYTES, 8)?;
        let start = self.reserve(0, 1)?;
        let facts = reader.string::<STORE>(|bytes| self.append(bytes))?;
        self.write(node, &(Kind::String as u32).to_le_bytes())?;
        self.write(node + 8, &self.address(start)?.to_le_bytes())?;
        self.write(node + 12, &facts.bytes.to_le_bytes())?;
        self.write(node + 16, &facts.scalars.unwrap_or(0).to_le_bytes())?;
        Ok(node)
    }
}
