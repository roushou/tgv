//! opencode: what Ren knows of it, in one place. How it is launched through
//! the gateway (`launch`); the attribution its requests carry (`headers`)
//! and the session identifier they name (`session`); driving it through its
//! Agent Client Protocol mode and listing its models (`acp`); its tools
//! (`tools`); and its own sign-in commands (`auth`). The rest of Ren reads opencode through the shared types these
//! fill.

mod acp;
mod auth;
mod headers;
mod launch;
mod session;
mod tools;

pub use acp::{OpenCodeAcp, OpenCodeModels};
pub use auth::OpenCodeAuth;
pub use tools::OpenCodeTool;
