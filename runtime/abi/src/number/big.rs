use core::cmp::Ordering;

/// Stack arithmetic for bounded decimal parsing and radix formatting. Capacity overflow traps.
#[derive(Clone)]
pub struct Big<const N: usize> {
    limbs: [u32; N],
    len: usize,
}

impl<const N: usize> Big<N> {
    pub const fn zero() -> Self {
        Self {
            limbs: [0; N],
            len: 0,
        }
    }

    pub fn from_u64(value: u64) -> Self {
        let mut result = Self::zero();
        if value != 0 {
            result.limbs[0] = value as u32;
            result.len = 1;
            if value >> 32 != 0 {
                result.limbs[1] = (value >> 32) as u32;
                result.len = 2;
            }
        }
        result
    }

    pub fn is_zero(&self) -> bool {
        self.len == 0
    }

    fn normalize(&mut self) {
        while self.len > 0 && self.limbs[self.len - 1] == 0 {
            self.len -= 1;
        }
    }

    pub fn mul_add(&mut self, factor: u32, addend: u32) {
        let mut carry = u64::from(addend);
        for limb in &mut self.limbs[..self.len] {
            let product = u64::from(*limb) * u64::from(factor) + carry;
            *limb = product as u32;
            carry = product >> 32;
        }
        if carry != 0 {
            self.limbs[self.len] = carry as u32;
            self.len += 1;
        }
        self.normalize();
    }

    pub fn shl(&mut self, bits: u32) {
        if self.is_zero() || bits == 0 {
            return;
        }
        let (words, bits) = ((bits / 32) as usize, bits % 32);
        let old_len = self.len;
        let carry = if bits == 0 {
            0
        } else {
            self.limbs[old_len - 1] >> (32 - bits)
        };
        let len = old_len + words + usize::from(carry != 0);
        assert!(len <= N, "big integer capacity exceeded");
        if carry != 0 {
            self.limbs[len - 1] = carry;
        }
        for index in (0..old_len).rev() {
            let low = if bits != 0 && index > 0 {
                self.limbs[index - 1] >> (32 - bits)
            } else {
                0
            };
            self.limbs[index + words] = (self.limbs[index] << bits) | low;
        }
        self.limbs[..words].fill(0);
        self.len = len;
    }

    pub fn div_rem_u32(&mut self, divisor: u32) -> u32 {
        let mut remainder = 0u64;
        for limb in self.limbs[..self.len].iter_mut().rev() {
            let value = (remainder << 32) | u64::from(*limb);
            *limb = (value / u64::from(divisor)) as u32;
            remainder = value % u64::from(divisor);
        }
        self.normalize();
        remainder as u32
    }

    pub fn compare(&self, other: &Self) -> Ordering {
        self.len.cmp(&other.len).then_with(|| {
            self.limbs[..self.len]
                .iter()
                .rev()
                .cmp(other.limbs[..other.len].iter().rev())
        })
    }

    pub fn add_assign(&mut self, other: &Self) {
        let len = self.len.max(other.len);
        let mut carry = 0u64;
        for index in 0..len {
            let left = if index < self.len {
                u64::from(self.limbs[index])
            } else {
                0
            };
            let right = if index < other.len {
                u64::from(other.limbs[index])
            } else {
                0
            };
            let sum = left + right + carry;
            self.limbs[index] = sum as u32;
            carry = sum >> 32;
        }
        self.len = len;
        if carry != 0 {
            self.limbs[self.len] = carry as u32;
            self.len += 1;
        }
    }

    fn sub_assign(&mut self, other: &Self) {
        let mut borrow = 0i64;
        for index in 0..self.len {
            let right = if index < other.len {
                i64::from(other.limbs[index])
            } else {
                0
            };
            let difference = i64::from(self.limbs[index]) - right - borrow;
            self.limbs[index] = difference as u32;
            borrow = i64::from(difference < 0);
        }
        self.normalize();
    }

    /// A radix digit's quotient: callers bound the result by the radix.
    pub fn div_rem_small_quot(&mut self, divisor: &Self) -> u32 {
        assert!(!divisor.is_zero(), "division by zero");
        let mut quotient = 0;
        while self.compare(divisor) != Ordering::Less {
            self.sub_assign(divisor);
            quotient += 1;
        }
        quotient
    }
}
