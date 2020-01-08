use super::commands::debugger;
use wasminspect_core::vm::{
    resolve_func_addr, Either, Executor, ModuleIndex, Store, WasmError, WasmInstance, WasmValue,
    CallFrame, ProgramCounter, Signal, InstIndex
};
use wasminspect_wasi::instantiate_wasi;

pub struct MainDebugger<'a> {
    executor: Option<Executor<'a>>,
    store: Store,
    instance: WasmInstance,
    module_index: Option<ModuleIndex>,
}

impl<'a> MainDebugger<'a> {
    pub fn new(file: Option<String>) -> Result<Self, String> {
        let mut instance = WasmInstance::new();
        let (ctx, wasi_snapshot_preview) = instantiate_wasi();

        let mut store = Store::new();
        store.add_embed_context(Box::new(ctx));
        store.load_host_module("wasi_snapshot_preview1".to_string(), wasi_snapshot_preview);

        let module_index = if let Some(file) = file {
            let parity_module = parity_wasm::deserialize_file(file).unwrap();
            Some(
                store
                    .load_parity_module(None, parity_module)
                    .map_err(|err| format!("{}", err))?,
            )
        } else {
            None
        };
        Ok(Self {
            executor: None,
            store: store,
            instance,
            module_index,
        })
    }
}

impl<'a> debugger::Debugger for MainDebugger<'a> {
    fn run(&mut self, name: Option<String>) -> Result<Vec<WasmValue>, String> {
        if let Some(module_index) = self.module_index {
            let module = self.store.module(module_index).defined().unwrap();
            let func_addr = if let Some(func_name) = name {
                if let Some(Some(func_addr)) = module.exported_func(func_name.clone()).ok() {
                    func_addr
                } else {
                    return Err(format!("Entry function {} not found", func_name));
                }
            } else if let Some(start_func_addr) = module.start_func_addr() {
                *start_func_addr
            } else {
                if let Some(Some(func_addr)) = module.exported_func("_start".to_string()).ok() {
                    func_addr
                } else {
                    return Err(format!("Entry function _start not found"));
                }
            };
            let resolved_addr = resolve_func_addr(func_addr, &self.store)
                .map_err(|e| format!("Failed to execute {}", e))?;
            match resolved_addr {
                Either::Right(host_func_body) => {
                    let mut results = Vec::new();
                    match host_func_body.call(&vec![], &mut results, &self.store, func_addr.0) {
                        Ok(_) => return Ok(results),
                        Err(_) => return Err(format!("Failed to execute host func")),
                    }
                }
                Either::Left((func_addr, func)) => {
                    let (frame, ret_types) = {
                        let ret_types =
                            func.ty().return_type().map(|ty| vec![ty]).unwrap_or(vec![]);
                        let frame = CallFrame::new_from_func(func_addr, func, vec![], None);
                        (frame, ret_types)
                    };
                    let pc = ProgramCounter::new(func_addr, InstIndex::zero());
                    self.executor = Some(Executor::new(frame, ret_types.len(), pc, &mut self.store));
                    loop {
                        let result = self.executor.unwrap().execute_step();
                        match result {
                            Ok(Signal::Next) => continue,
                            Ok(Signal::End) => match self.executor.unwrap().pop_result(ret_types) {
                                Ok(values) => return Ok(values),
                                Err(err) => return Err(format!("Return value failure {:?}", err)),
                            },
                            Err(err) => return Err(format!("Function execc failure {:?}", err)),
                        }
                    }
                }
            }
        } else {
            Err("No module loaded".to_string())
        }
    }
}