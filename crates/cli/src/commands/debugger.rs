use wasminspect_core::vm::WasmValue;
pub trait Debugger<'a> {
    fn run(&mut self, name: Option<String>) -> Result<Vec<WasmValue>, String>;
}