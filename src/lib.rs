//! Graphics preset validation and planning independent of the Windows hook.

mod preset;

pub use preset::{
    active_hash, hash_key, parse_preset, plan, Change, Platform, Preset, PresetError, RawRow,
    Selection,
};

#[cfg(windows)]
mod windows;
