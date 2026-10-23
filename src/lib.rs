//! Rust Audacity audio engine (GUI-independent): DSP, effects, file I/O and the
//! project/track model. Used by the desktop app and by the MCP server.

pub mod batch;
pub mod bridge;
pub mod dsp;
pub mod effects;
pub mod envelope;
pub mod fileio;
pub mod labels;
pub mod mcp;
pub mod metadata;
pub mod ollama;
pub mod plugins;
pub mod project;
pub mod projectfile;
pub mod recovery;
pub mod track;
pub mod transcript;
pub mod whisper;
