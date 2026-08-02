//! 错误类型与 C ABI 状态码。

use std::fmt;

/// C ABI 状态码(与 `include/flow.h` 的 `LMFLOW_*` 常量一一对应)。
pub mod code {
    pub const OK: i32 = 0;
    pub const INVALID_ARG: i32 = 1;
    pub const NOT_FOUND: i32 = 2;
    pub const KERNEL: i32 = 3;
    pub const PANIC: i32 = 4;
    pub const WOULD_BLOCK: i32 = 5;
    pub const TIMEOUT: i32 = 6;
    pub const CANCELLED: i32 = 7;
    pub const CLOSED: i32 = 8;
    pub const ABI: i32 = 9;
    pub const UNSUPPORTED: i32 = 10;
    pub const STATE: i32 = 11;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 配置 / 参数非法(端口名未定义、拓扑不合法等)。
    InvalidArg(String),
    /// 名字查不到(算子未注册、端口不存在)。
    NotFound(String),
    /// 算子回调返回失败或抛异常。
    Kernel(String),
    /// 跨 FFI 的 panic 兜底。
    Panic(String),
    /// 非阻塞接口:队列满 / 暂无数据。
    WouldBlock,
    Timeout,
    Cancelled,
    /// 端口已关闭 / 图已终止。
    Closed,
    /// ABI 版本不匹配。
    Abi(String),
    /// 配置用到了本版本尚未实现的特性 —— 宁可报错也不静默忽略。
    Unsupported(String),
    /// 图状态不允许该操作。
    State(String),
}

impl Error {
    pub fn code(&self) -> i32 {
        match self {
            Error::InvalidArg(_) => code::INVALID_ARG,
            Error::NotFound(_) => code::NOT_FOUND,
            Error::Kernel(_) => code::KERNEL,
            Error::Panic(_) => code::PANIC,
            Error::WouldBlock => code::WOULD_BLOCK,
            Error::Timeout => code::TIMEOUT,
            Error::Cancelled => code::CANCELLED,
            Error::Closed => code::CLOSED,
            Error::Abi(_) => code::ABI,
            Error::Unsupported(_) => code::UNSUPPORTED,
            Error::State(_) => code::STATE,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidArg(m) => write!(f, "invalid argument or config: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Kernel(m) => write!(f, "kernel failed: {m}"),
            Error::Panic(m) => write!(f, "internal panic: {m}"),
            Error::WouldBlock => f.write_str("temporarily unavailable (queue full or no data)"),
            Error::Timeout => f.write_str("timeout"),
            Error::Cancelled => f.write_str("cancelled"),
            Error::Closed => f.write_str("closed"),
            Error::Abi(m) => write!(f, "ABI mismatch: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported in this version: {m}"),
            Error::State(m) => write!(f, "operation not allowed in current state: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// 便捷构造。
pub fn invalid_arg<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::InvalidArg(msg.into()))
}
pub fn not_found<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::NotFound(msg.into()))
}
pub fn unsupported<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Unsupported(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_c_abi() {
        assert_eq!(Error::InvalidArg(String::new()).code(), 1);
        assert_eq!(Error::Unsupported(String::new()).code(), 10);
        assert_eq!(Error::State(String::new()).code(), 11);
    }

    #[test]
    fn messages_are_non_empty() {
        for e in [
            Error::WouldBlock,
            Error::Timeout,
            Error::Cancelled,
            Error::Closed,
        ] {
            assert!(!e.to_string().is_empty());
        }
    }
}
