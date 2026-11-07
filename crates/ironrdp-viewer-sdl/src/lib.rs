#![cfg_attr(doc, doc = include_str!("../README.md"))]
#![expect(
    unused_crate_dependencies,
    reason = "dependencies are shared by the library and binary targets"
)]

pub mod desktop;
pub mod graphics;
pub mod input;
pub mod renderer;
