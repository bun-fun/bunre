#![warn(unused_must_use)]

pub mod internal_jsc;
#[path = "JSSourceMap.rs"]
pub mod js_source_map;

// ---- public surface ---------------------------------------------------------

pub use js_source_map::JSSourceMap;
