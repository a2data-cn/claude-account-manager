//! 账号管理的核心逻辑：数据目录、凭据切换、用量、规则、HTTP 路由。
//! 不依赖 Tauri，可以单独编译和测试（cargo test -p cam-engine）。
//! 和脚本版 scripts/cam.py 读写同一份数据，行为保持一致，见 tests/shared 里两边共用的测试数据。

pub mod api;
pub mod browser;
pub mod claude;
pub mod desktop_app;
pub mod desktop_usage;
pub mod health;
pub mod identity;
pub mod mailbox;
pub mod rules;
pub mod store;
pub mod usage;
pub mod vault;
