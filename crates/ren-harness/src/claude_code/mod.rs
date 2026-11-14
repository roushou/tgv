//! Claude Code: what Ren knows of it, in one place. Its print mode with
//! streaming JSON and how Ren drives it and lists its models
//! (`stream_json`); how it is launched through the gateway (`launch`); the
//! attribution its requests carry (`headers`); its tools (`tools`); and its
//! own sign-in commands (`auth`).
//! The rest of Ren reads Claude Code through the shared types these fill.

mod auth;
mod headers;
mod launch;
mod stream_json;
mod tools;

pub use auth::ClaudeAuth;
pub use stream_json::{ClaudeModels, ClaudeStreamJson};
pub use tools::ClaudeCodeTool;
