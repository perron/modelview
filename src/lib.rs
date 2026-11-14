//! modelview library: load a 3D scene, inspect it, render it, export glTF.
//!
//! The command-line front end is `modelview`. Library callers, including the
//! loader child process, use [`load_in_process`]. The CLI and MCP server load
//! through a child process so a malformed file cannot take them down.

pub mod cli;
pub mod error;
pub mod export_glb;
pub mod gui;
pub mod info;
pub mod load;
pub mod mcp_server;
pub mod render;
pub mod scene;
pub mod validate;

pub use error::{MvError, Result};
pub use load::{load_in_process, load_isolated};
pub use scene::Scene;
