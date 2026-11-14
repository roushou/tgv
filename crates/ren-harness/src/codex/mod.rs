//! Codex: what Ren knows of it, in one place. Its app server
//! (`codex app-server`) and how Ren drives it and lists its models
//! (`app_server`); how it is launched through the gateway (`launch`); the
//! attribution its requests carry (`headers`); its tools (`tools`); and its
//! own sign-in commands (`auth`).
//! The rest of Ren reads Codex through the shared types these fill.

mod app_server;
mod auth;
mod headers;
mod launch;
mod tools;

pub use app_server::{CodexAppServer, CodexModels};
pub use auth::CodexAuth;
pub use tools::CodexTool;
