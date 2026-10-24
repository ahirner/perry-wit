//! Numeric option and accessor codes shared by Temporal lowering and its checked codec.
macro_rules! codes {
    ($name:ident { $($variant:ident = $code:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        #[repr(u32)]
        pub enum $name { $($variant = $code),+ }

        impl TryFrom<u32> for $name {
            type Error = u32;
            fn try_from(value: u32) -> Result<Self, u32> {
                match value {
                    $($code => Ok(Self::$variant),)+
                    other => Err(other),
                }
            }
        }
    };
}

codes!(Overflow { Reject = 0, Constrain = 1 });
codes!(Unit { Day = 0, Week = 1, Month = 2, Year = 3 });
codes!(DurationPart { Years = 0, Months = 1, Weeks = 2, Days = 3 });
codes!(DatePart {
    Year = 0, Month = 1, Day = 2, Hour = 3, Minute = 4, Second = 5,
    Millisecond = 6, Microsecond = 7, Nanosecond = 8, Weekday = 9,
});
