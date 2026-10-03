use super::Error;

pub(super) trait Output {
    fn reserve(&mut self, size: usize, alignment: usize) -> Result<usize, Error>;
    fn write(&mut self, position: usize, bytes: &[u8]) -> Result<(), Error>;
    fn address(&self, position: usize) -> Result<u32, Error>;
}

pub(super) struct Measure {
    pub(super) length: usize,
}

impl Output for Measure {
    fn reserve(&mut self, size: usize, alignment: usize) -> Result<usize, Error> {
        let start = self
            .length
            .checked_add(alignment - 1)
            .ok_or(Error::Capacity)?
            & !(alignment - 1);
        self.length = start.checked_add(size).ok_or(Error::Capacity)?;
        u32::try_from(self.length).map_err(|_| Error::Capacity)?;
        Ok(start)
    }
    fn write(&mut self, _position: usize, _bytes: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    fn address(&self, position: usize) -> Result<u32, Error> {
        u32::try_from(position).map_err(|_| Error::Capacity)
    }
}

pub(super) struct Buffer<'a> {
    pub(super) bytes: &'a mut [u8],
    pub(super) length: usize,
    pub(super) base: u32,
}

impl Output for Buffer<'_> {
    fn reserve(&mut self, size: usize, alignment: usize) -> Result<usize, Error> {
        let mut measured = Measure {
            length: self.length,
        };
        let start = measured.reserve(size, alignment)?;
        self.bytes
            .get_mut(self.length..measured.length)
            .ok_or(Error::Capacity)?
            .fill(0);
        self.length = measured.length;
        Ok(start)
    }
    fn write(&mut self, position: usize, bytes: &[u8]) -> Result<(), Error> {
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
}
