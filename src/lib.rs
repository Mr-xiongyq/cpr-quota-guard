//! 账号额度保护策略与公开 SDK 处理器的组合入口

mod app;
pub mod config;
mod maintenance;
mod management;
pub mod policy;

pub use app::build;
