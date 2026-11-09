//! 常駐アプリと検証から共有する実装。単体テストはこのライブラリで一度だけ実行する。

mod app;
#[doc(hidden)]
pub use app::replay;
pub use app::run;

pub mod asr;
pub mod audio;
pub(crate) mod chatgpt;
pub mod config;
pub mod context;
pub mod feedback;
pub mod format;
pub(crate) mod history;
pub(crate) mod hook;
pub mod lexicon;
pub mod overlay;
pub mod paste;
pub(crate) mod provider;
pub(crate) mod recovery;
pub mod segment;
pub(crate) mod settings;
pub(crate) mod sound;
pub(crate) mod startup;
pub(crate) mod state;
pub mod uia;
pub(crate) mod usage;
