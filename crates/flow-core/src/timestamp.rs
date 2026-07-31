//! 时间戳:整条数据流的排序与生命周期标记。
//!
//! 数值空间(i64)划分为若干哨兵值 + 中间的合法区间:
//! ```text
//!  MIN      MIN+1       MIN+2      MIN+3  ...  MAX-3       MAX-2        MAX-1              MAX
//!  Unset  Unstarted   PreStream    Min          Max     PostStream  OneOverPostStream    Done
//!         └────────── 不允许出现在流中 ──────────┘         └────── 不允许出现在流中 ──────┘
//!                                 └── 合法数据区间 ──┘
//! ```
//! - `PreStream` / `PostStream`:流首 / 流尾的特殊单包位置。
//! - `Done`:端口已关闭且不会再有数据。
//! - `Unset`:未赋值(默认值)。

use std::fmt;

/// 带哨兵语义的时间戳。内层 `i64` 公开,便于与 C ABI 直接互转。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Timestamp(pub i64);

impl Default for Timestamp {
    /// 默认为 `Unset`(与 C ABI 的 `FLOW_TS_UNSET` 一致)。
    fn default() -> Self {
        Self::unset()
    }
}

impl Timestamp {
    pub const fn unset() -> Self {
        Timestamp(i64::MIN)
    }
    pub const fn unstarted() -> Self {
        Timestamp(i64::MIN + 1)
    }
    pub const fn pre_stream() -> Self {
        Timestamp(i64::MIN + 2)
    }
    pub const fn min() -> Self {
        Timestamp(i64::MIN + 3)
    }
    pub const fn max() -> Self {
        Timestamp(i64::MAX - 3)
    }
    pub const fn post_stream() -> Self {
        Timestamp(i64::MAX - 2)
    }
    pub const fn one_over_post_stream() -> Self {
        Timestamp(i64::MAX - 1)
    }
    pub const fn done() -> Self {
        Timestamp(i64::MAX)
    }

    /// 是否落在普通数据区间 `[Min, Max]`。
    pub fn is_range_value(self) -> bool {
        self >= Self::min() && self <= Self::max()
    }

    /// 是否允许作为流中数据包的时间戳(含 PreStream / PostStream)。
    pub fn is_allowed_in_stream(self) -> bool {
        self >= Self::pre_stream() && self <= Self::post_stream()
    }

    /// 本时间戳之后、下一个允许出现在流中的时间戳。
    ///
    /// `PreStream` 之后不允许再有包,故返回 `OneOverPostStream`。
    pub fn next_allowed_in_stream(self) -> Self {
        if self >= Self::max() || self == Self::pre_stream() {
            Self::one_over_post_stream()
        } else if self < Self::min() {
            Self::min()
        } else {
            Timestamp(self.0 + 1)
        }
    }

    pub fn has_next_allowed_in_stream(self) -> bool {
        self < Self::max() && self != Self::pre_stream()
    }

    pub fn previous_allowed_in_stream(self) -> Self {
        if self <= Self::min() || self == Self::post_stream() {
            Self::unstarted()
        } else if self > Self::max() {
            Self::max()
        } else {
            Timestamp(self.0 - 1)
        }
    }
}

/// 加减用饱和运算:哨兵值附近不会因溢出而 panic 或回绕。
impl std::ops::Add<i64> for Timestamp {
    type Output = Timestamp;
    fn add(self, rhs: i64) -> Timestamp {
        Timestamp(self.0.saturating_add(rhs))
    }
}

impl std::ops::Sub<i64> for Timestamp {
    type Output = Timestamp;
    fn sub(self, rhs: i64) -> Timestamp {
        Timestamp(self.0.saturating_sub(rhs))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            t if t == Self::unset() => "Unset",
            t if t == Self::unstarted() => "Unstarted",
            t if t == Self::pre_stream() => "PreStream",
            t if t == Self::post_stream() => "PostStream",
            t if t == Self::one_over_post_stream() => "OneOverPostStream",
            t if t == Self::done() => "Done",
            _ => return write!(f, "{}", self.0),
        };
        f.write_str(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinels_are_ordered() {
        assert!(Timestamp::unset() < Timestamp::unstarted());
        assert!(Timestamp::unstarted() < Timestamp::pre_stream());
        assert!(Timestamp::pre_stream() < Timestamp::min());
        assert!(Timestamp::min() < Timestamp::max());
        assert!(Timestamp::max() < Timestamp::post_stream());
        assert!(Timestamp::post_stream() < Timestamp::one_over_post_stream());
        assert!(Timestamp::one_over_post_stream() < Timestamp::done());
    }

    #[test]
    fn default_is_unset() {
        assert_eq!(Timestamp::default(), Timestamp::unset());
    }

    #[test]
    fn allowed_in_stream_range() {
        assert!(Timestamp::pre_stream().is_allowed_in_stream());
        assert!(Timestamp(0).is_allowed_in_stream());
        assert!(Timestamp::post_stream().is_allowed_in_stream());
        assert!(!Timestamp::unset().is_allowed_in_stream());
        assert!(!Timestamp::unstarted().is_allowed_in_stream());
        assert!(!Timestamp::one_over_post_stream().is_allowed_in_stream());
        assert!(!Timestamp::done().is_allowed_in_stream());
    }

    #[test]
    fn range_value_only_covers_data_interval() {
        assert!(Timestamp(0).is_range_value());
        assert!(Timestamp::min().is_range_value());
        assert!(Timestamp::max().is_range_value());
        assert!(!Timestamp::pre_stream().is_range_value());
        assert!(!Timestamp::post_stream().is_range_value());
    }

    #[test]
    fn next_allowed_in_stream_cases() {
        assert_eq!(Timestamp(5).next_allowed_in_stream(), Timestamp(6));
        // PreStream 之后不允许再有包
        assert_eq!(
            Timestamp::pre_stream().next_allowed_in_stream(),
            Timestamp::one_over_post_stream()
        );
        assert_eq!(
            Timestamp::max().next_allowed_in_stream(),
            Timestamp::one_over_post_stream()
        );
        assert_eq!(
            Timestamp::post_stream().next_allowed_in_stream(),
            Timestamp::one_over_post_stream()
        );
        // 低于 Min 的(非 PreStream)哨兵推进到 Min
        assert_eq!(
            Timestamp::unstarted().next_allowed_in_stream(),
            Timestamp::min()
        );
        assert_eq!(
            Timestamp::unset().next_allowed_in_stream(),
            Timestamp::min()
        );
    }

    #[test]
    fn previous_allowed_in_stream_cases() {
        assert_eq!(Timestamp(5).previous_allowed_in_stream(), Timestamp(4));
        assert_eq!(
            Timestamp::min().previous_allowed_in_stream(),
            Timestamp::unstarted()
        );
        assert_eq!(
            Timestamp::post_stream().previous_allowed_in_stream(),
            Timestamp::unstarted()
        );
        assert_eq!(
            Timestamp::done().previous_allowed_in_stream(),
            Timestamp::max()
        );
        assert_eq!(
            Timestamp::one_over_post_stream().previous_allowed_in_stream(),
            Timestamp::max()
        );
    }

    #[test]
    fn has_next_allowed_in_stream() {
        assert!(Timestamp(0).has_next_allowed_in_stream());
        assert!(!Timestamp::pre_stream().has_next_allowed_in_stream());
        assert!(!Timestamp::max().has_next_allowed_in_stream());
        assert!(!Timestamp::done().has_next_allowed_in_stream());
    }

    #[test]
    fn arithmetic_saturates_instead_of_overflowing() {
        assert_eq!(Timestamp::done() + 1, Timestamp::done());
        assert_eq!(Timestamp::unset() - 1, Timestamp::unset());
        assert_eq!(Timestamp(10) + 5, Timestamp(15));
        assert_eq!(Timestamp(10) - 5, Timestamp(5));
    }

    #[test]
    fn display_names_sentinels() {
        assert_eq!(Timestamp::done().to_string(), "Done");
        assert_eq!(Timestamp::pre_stream().to_string(), "PreStream");
        assert_eq!(Timestamp(7).to_string(), "7");
    }
}
