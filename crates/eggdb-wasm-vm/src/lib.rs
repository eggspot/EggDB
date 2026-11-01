//! EggDB's WebAssembly interpreter (D232), written from the WebAssembly 1.0 core specification,
//! for functions users write in any language that compiles to WebAssembly. [`Module::parse`]
//! reads the binary format (types, functions, one memory, globals, exports, code and data) and
//! refuses what it does not run yet; [`Instance`] runs exported functions over the integer
//! instruction set, with every run bounded by [`Limits`]: fuel per instruction, memory pages and
//! call depth, so a function can neither hang the server nor exhaust its memory (D45). Floating
//! point, tables and imports are refused at parse time for now.

mod decode;
mod run;

pub use decode::{Module, WasmError};
pub use run::{Instance, Limits, Trap, Value};

#[cfg(test)]
mod tests;
