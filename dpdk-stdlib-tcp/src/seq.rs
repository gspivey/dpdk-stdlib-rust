//! TCP sequence number with modulo-2³² serial-number arithmetic (RFC 9293 §3.4).
//!
//! `Ord` is intentionally NOT implemented — serial number comparison is
//! non-transitive over the full u32 range. Use `lt`/`le`/`gt`/`in_range` only.

/// A 32-bit TCP sequence number with modular arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SeqNum(pub u32);

impl SeqNum {
    /// Serial-number less-than (RFC 1982 / RFC 9293 §3.4).
    /// `self < other` iff `other - self` is in (0, 2^31).
    #[inline]
    pub fn lt(self, other: SeqNum) -> bool {
        let diff = other.0.wrapping_sub(self.0);
        diff != 0 && diff < (1 << 31)
    }

    /// Serial-number less-than-or-equal.
    #[inline]
    pub fn le(self, other: SeqNum) -> bool {
        self == other || self.lt(other)
    }

    /// Serial-number greater-than.
    #[inline]
    pub fn gt(self, other: SeqNum) -> bool {
        other.lt(self)
    }

    /// Add an offset (wrapping).
    #[inline]
    pub fn add(self, offset: u32) -> SeqNum {
        SeqNum(self.0.wrapping_add(offset))
    }

    /// Unsigned distance from `other` to `self` (self - other, wrapping).
    /// Only meaningful when `self >= other` in serial space.
    #[inline]
    pub fn diff(self, other: SeqNum) -> u32 {
        self.0.wrapping_sub(other.0)
    }

    /// Check if `self` is in the range `[start, end)` in serial space.
    #[inline]
    pub fn in_range(self, start: SeqNum, end: SeqNum) -> bool {
        start.le(self) && self.lt(end)
    }
}

impl std::fmt::Display for SeqNum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_less_than() {
        assert!(SeqNum(0).lt(SeqNum(1)));
        assert!(SeqNum(100).lt(SeqNum(200)));
        assert!(!SeqNum(200).lt(SeqNum(100)));
        assert!(!SeqNum(5).lt(SeqNum(5)));
    }

    #[test]
    fn wrap_around() {
        // Near u32::MAX, wrapping forward is "greater"
        assert!(SeqNum(u32::MAX - 1).lt(SeqNum(u32::MAX)));
        assert!(SeqNum(u32::MAX).lt(SeqNum(0)));
        assert!(SeqNum(u32::MAX).lt(SeqNum(1)));
    }

    #[test]
    fn le_and_gt() {
        assert!(SeqNum(5).le(SeqNum(5)));
        assert!(SeqNum(5).le(SeqNum(6)));
        assert!(!SeqNum(6).le(SeqNum(5)));
        assert!(SeqNum(6).gt(SeqNum(5)));
        assert!(!SeqNum(5).gt(SeqNum(5)));
    }

    #[test]
    fn add_wraps() {
        assert_eq!(SeqNum(u32::MAX).add(1), SeqNum(0));
        assert_eq!(SeqNum(u32::MAX).add(5), SeqNum(4));
        assert_eq!(SeqNum(0).add(100), SeqNum(100));
    }

    #[test]
    fn diff_wraps() {
        assert_eq!(SeqNum(10).diff(SeqNum(5)), 5);
        assert_eq!(SeqNum(0).diff(SeqNum(u32::MAX)), 1);
        assert_eq!(SeqNum(5).diff(SeqNum(u32::MAX - 4)), 10);
    }

    #[test]
    fn in_range() {
        assert!(SeqNum(5).in_range(SeqNum(5), SeqNum(10)));
        assert!(SeqNum(9).in_range(SeqNum(5), SeqNum(10)));
        assert!(!SeqNum(10).in_range(SeqNum(5), SeqNum(10)));
        assert!(!SeqNum(4).in_range(SeqNum(5), SeqNum(10)));
    }

    #[test]
    fn in_range_wrap() {
        // Range wrapping around u32::MAX
        assert!(SeqNum(0).in_range(SeqNum(u32::MAX - 2), SeqNum(3)));
        assert!(SeqNum(u32::MAX).in_range(SeqNum(u32::MAX - 2), SeqNum(3)));
        assert!(!SeqNum(3).in_range(SeqNum(u32::MAX - 2), SeqNum(3)));
    }

    #[test]
    fn transitivity_within_half_space() {
        // Within the valid comparison window (< 2^31 apart), transitivity holds
        let a = SeqNum(100);
        let b = SeqNum(200);
        let c = SeqNum(300);
        assert!(a.lt(b));
        assert!(b.lt(c));
        assert!(a.lt(c));
    }

    #[test]
    fn n_lt_n_plus_1() {
        // For any n, n < n+1 holds
        for n in [0u32, 1, 100, u32::MAX / 2, u32::MAX - 1, u32::MAX] {
            assert!(SeqNum(n).lt(SeqNum(n).add(1)));
        }
    }
}
