use super::address::{FuncAddr, GlobalAddr, MemoryAddr, TableAddr};
use super::func::*;
use super::host::BuiltinPrintI32;
use super::memory::*;
use super::module::*;
use super::stack::*;
use super::store::*;
use super::value::*;
use parity_wasm::elements::{BlockType, FunctionType, InitExpr, Instruction, ValueType};

use std::convert::TryFrom;
use std::ops::*;

#[derive(Debug)]
pub enum ExecError {
    Panic(String),
    NoCallFrame,
}

pub enum ExecSuccess {
    Next,
    End,
}

pub type ExecResult = Result<ExecSuccess, ExecError>;

#[derive(Debug)]
pub enum ReturnValError {
    TypeMismatchReturnValue(Value, ValueType),
    NoValue(ValueType),
    NoCallFrame,
}

pub type ReturnValResult = Result<Vec<Value>, ReturnValError>;

pub struct Executor<'a> {
    store: &'a mut Store,
    pc: ProgramCounter,
    stack: Stack,
}

impl<'a> Executor<'a> {
    pub fn new(
        local_len: usize,
        func_addr: FuncAddr,
        initial_args: Vec<Value>,
        initial_arity: usize,
        pc: ProgramCounter,
        store: &'a mut Store,
    ) -> Self {
        let mut stack = Stack::default();
        let frame = CallFrame::new(func_addr, local_len, initial_args, None);
        let f = CallFrame::new(func_addr, local_len, vec![], None);
        stack.set_frame(frame);
        stack.push_label(Label::Return(initial_arity));
        Self { store, pc, stack }
    }

    pub fn pop_result(&mut self, return_ty: Vec<ValueType>) -> ReturnValResult {
        let mut results = vec![];
        for ty in return_ty {
            let val = self.stack.pop_value();
            results.push(val);
            if val.value_type() != ty {
                return Err(ReturnValError::TypeMismatchReturnValue(val.clone(), ty));
            }
        }
        Ok(results)
    }

    pub fn current_func_insts(&self) -> &[Instruction] {
        let func = self.store.func(self.stack.current_func_addr());
        &func.defined().unwrap().code().instructions()
    }

    pub fn execute_step(&mut self) -> ExecResult {
        let func = self.store.func(self.pc.func_addr()).defined().unwrap();
        let module_index = func.module_index().clone();
        let inst = func.code().inst(self.pc.inst_index()).clone();
        return self.execute_inst(&inst, module_index);
    }

    fn execute_inst(&mut self, inst: &Instruction, module_index: ModuleIndex) -> ExecResult {
        self.pc.inc_inst_index();
        {
            let mut indent = String::new();
            for _ in 0..self.stack.current_frame_labels().len() {
                indent.push_str("  ");
            }
            println!("{}{}", indent, inst.clone());
        }
        println!("{:?}", self.stack);
        let result = match inst {
            Instruction::Unreachable => panic!(),
            Instruction::Nop => Ok(ExecSuccess::Next),
            Instruction::Block(ty) => {
                self.stack.push_label(Label::Block({
                    match ty {
                        BlockType::Value(_) => 1,
                        BlockType::NoResult => 0,
                    }
                }));
                Ok(ExecSuccess::Next)
            }
            Instruction::Loop(_) => {
                let start_loop = InstIndex(self.pc.inst_index().0 - 1);
                self.stack.push_label(Label::new_loop(start_loop));
                Ok(ExecSuccess::Next)
            }
            Instruction::If(ty) => {
                let val: i32 = self.pop_as();
                self.stack.push_label(Label::If(match ty {
                    BlockType::Value(_) => 1,
                    BlockType::NoResult => 0,
                }));
                if val == 0 {
                    let mut depth = 1;
                    loop {
                        let index = self.pc.inst_index().0 as usize;
                        match self.current_func_insts()[index] {
                            Instruction::End => depth -= 1,
                            Instruction::Block(_) => depth += 1,
                            Instruction::If(_) => depth += 1,
                            Instruction::Loop(_) => depth += 1,
                            Instruction::Else => {
                                if depth == 1 {
                                    self.pc.inc_inst_index();
                                    break;
                                }
                            }
                            _ => (),
                        }
                        if depth == 0 {
                            break;
                        }
                        self.pc.inc_inst_index();
                    }
                }
                Ok(ExecSuccess::Next)
            }
            Instruction::Else => self.branch(0),
            Instruction::End => {
                if self.stack.is_func_top_level() {
                    // When the end of a function is reached without a jump
                    let frame = self.stack.current_frame().clone();
                    let func = self.store.func(frame.func_addr);
                    println!("--- End of function {:?} ---", func.ty());
                    let arity = func.ty().return_type().map(|_| 1).unwrap_or(0);
                    let mut result = vec![];
                    for _ in 0..arity {
                        result.push(self.stack.pop_value());
                    }
                    // println!("{:?}", self.stack);
                    self.stack.pop_label();
                    self.stack.pop_frame();
                    for v in result {
                        self.stack.push_value(v);
                    }
                    println!("--- End of finish process ---");
                    if let Some(ret_pc) = frame.ret_pc {
                        self.pc = ret_pc;
                        Ok(ExecSuccess::Next)
                    } else {
                        Ok(ExecSuccess::End)
                    }
                } else {
                    // When the end of a block is reached without a jump
                    let results = self.stack.pop_while(|v| match v {
                        StackValue::Value(_) => true,
                        _ => false,
                    });
                    self.stack.pop_label();
                    for v in results {
                        self.stack.push_value(*v.as_value().unwrap());
                    }
                    Ok(ExecSuccess::Next)
                }
            }
            Instruction::Br(depth) => self.branch(*depth),
            Instruction::BrIf(depth) => {
                let val = self.stack.pop_value();
                if val != Value::I32(0) {
                    self.branch(*depth)
                } else {
                    Ok(ExecSuccess::Next)
                }
            }
            Instruction::BrTable(ref payload) => {
                let val: i32 = self.pop_as();
                let val = val as usize;
                let depth = if val < payload.table.len() {
                    payload.table[val]
                } else {
                    payload.default
                };
                self.branch(depth)
            }
            Instruction::Return => self.do_return(),
            Instruction::Call(func_index) => {
                let frame = self.stack.current_frame();
                let addr = FuncAddr(frame.module_index(), *func_index as usize);
                self.invoke(addr)
            }
            Instruction::CallIndirect(type_index, _) => {
                let (ty, addr) = {
                    let frame = self.stack.current_frame();
                    let addr = TableAddr(frame.module_index(), 0);
                    let module = self.store.module(frame.module_index()).defined().unwrap();
                    let ty = match module.get_type(*type_index as usize) {
                        parity_wasm::elements::Type::Function(ty) => ty,
                    };
                    (ty.clone(), addr)
                };
                let buf_index: i32 = self.pop_as();
                let table = self.store.table(addr);
                let buf_index = buf_index as usize;
                assert!(buf_index < table.buffer_len());
                let func_addr = match table.get_at(buf_index) {
                    Some(addr) => addr,
                    None => panic!(),
                };
                let func = self.store.func(func_addr);
                assert_eq!(*func.ty(), ty);
                self.invoke(func_addr)
            }
            Instruction::Drop => {
                self.stack.pop_value();
                Ok(ExecSuccess::Next)
            }
            Instruction::Select => {
                let cond: i32 = self.pop_as();
                let val2 = self.stack.pop_value();
                let val1 = self.stack.pop_value();
                if cond != 0 {
                    self.stack.push_value(val1);
                } else {
                    self.stack.push_value(val2);
                }
                Ok(ExecSuccess::Next)
            }
            Instruction::GetLocal(index) => {
                let value = self.stack.current_frame().local(*index as usize);
                self.stack.push_value(value);
                Ok(ExecSuccess::Next)
            }
            Instruction::SetLocal(index) => self.set_local(*index as usize),
            Instruction::TeeLocal(index) => {
                let val = self.stack.pop_value();
                self.stack.push_value(val);
                self.stack.push_value(val);
                self.set_local(*index as usize)
            }
            Instruction::GetGlobal(index) => {
                let addr = GlobalAddr(module_index, *index as usize);
                let global = self.store.global(addr);
                self.stack.push_value(global.value(self.store));
                Ok(ExecSuccess::Next)
            }
            Instruction::SetGlobal(index) => {
                let addr = GlobalAddr(module_index, *index as usize);
                let value = self.stack.pop_value();
                self.store.set_global(addr, value);
                Ok(ExecSuccess::Next)
            }

            Instruction::I32Load(_, offset) => self.load::<i32>(*offset as usize),
            Instruction::I64Load(_, offset) => self.load::<i64>(*offset as usize),
            Instruction::F32Load(_, offset) => self.load::<f32>(*offset as usize),
            Instruction::F64Load(_, offset) => self.load::<f64>(*offset as usize),

            Instruction::I32Load8S(_, offset) => self.load_extend::<i8, i32>(*offset as usize),
            Instruction::I32Load8U(_, offset) => self.load_extend::<u8, i32>(*offset as usize),
            Instruction::I32Load16S(_, offset) => self.load_extend::<i16, i32>(*offset as usize),
            Instruction::I32Load16U(_, offset) => self.load_extend::<u16, i32>(*offset as usize),

            Instruction::I64Load8S(_, offset) => self.load_extend::<i8, i64>(*offset as usize),
            Instruction::I64Load8U(_, offset) => self.load_extend::<u8, i64>(*offset as usize),
            Instruction::I64Load16S(_, offset) => self.load_extend::<i16, i64>(*offset as usize),
            Instruction::I64Load16U(_, offset) => self.load_extend::<u16, i64>(*offset as usize),
            Instruction::I64Load32S(_, offset) => self.load_extend::<i32, i64>(*offset as usize),
            Instruction::I64Load32U(_, offset) => self.load_extend::<u32, i64>(*offset as usize),

            Instruction::I32Store(_, offset) => self.store::<i32>(*offset as usize),
            Instruction::I64Store(_, offset) => self.store::<i64>(*offset as usize),
            Instruction::F32Store(_, offset) => self.store::<f32>(*offset as usize),
            Instruction::F64Store(_, offset) => self.store::<f64>(*offset as usize),

            Instruction::I32Store8(_, offset) => self.store_with_width::<i32>(*offset as usize, 8),
            Instruction::I32Store16(_, offset) => {
                self.store_with_width::<i32>(*offset as usize, 16)
            }
            Instruction::I64Store8(_, offset) => self.store_with_width::<i64>(*offset as usize, 8),
            Instruction::I64Store16(_, offset) => {
                self.store_with_width::<i64>(*offset as usize, 16)
            }
            Instruction::I64Store32(_, offset) => {
                self.store_with_width::<i64>(*offset as usize, 32)
            }

            Instruction::CurrentMemory(_) => unimplemented!(),
            Instruction::GrowMemory(_) => {
                let grow_page: i32 = self.pop_as();
                let frame = self.stack.current_frame();
                let mem_addr = MemoryAddr(frame.module_index(), 0);
                let mem = self.store.memory_mut(mem_addr);
                let size = match mem {
                    MemoryInstance::Defined(mem) => mem.page_count(),
                    MemoryInstance::External(mem) => panic!(),
                };
                match mem.grow(grow_page as usize) {
                    Ok(_) => {
                        self.stack.push_value(Value::I32(size as i32));
                    }
                    Err(err) => {
                        println!("[Debug] Failed to grow memory {:?}", err);
                        self.stack.push_value(Value::I32(-1));
                    }
                }
                Ok(ExecSuccess::Next)
            }

            Instruction::I32Const(val) => {
                self.stack.push_value(Value::I32(*val));
                Ok(ExecSuccess::Next)
            }
            Instruction::I64Const(val) => {
                self.stack.push_value(Value::I64(*val));
                Ok(ExecSuccess::Next)
            }
            Instruction::F32Const(val) => {
                self.stack.push_value(Value::F32(f32::from_bits(*val)));
                Ok(ExecSuccess::Next)
            }
            Instruction::F64Const(val) => {
                self.stack.push_value(Value::F64(f64::from_bits(*val)));
                Ok(ExecSuccess::Next)
            }

            Instruction::I32Eqz => self.testop::<i32, _>(|v| v == 0),
            Instruction::I32Eq => self.relop::<i32, _>(|a, b| a == b),
            Instruction::I32Ne => self.relop::<i32, _>(|a, b| a != b),
            Instruction::I32LtS => self.relop::<i32, _>(|a, b| a < b),
            Instruction::I32LtU => self.relop::<u32, _>(|a, b| a < b),
            Instruction::I32GtS => self.relop::<i32, _>(|a, b| a > b),
            Instruction::I32GtU => self.relop::<u32, _>(|a, b| a > b),
            Instruction::I32LeS => self.relop::<i32, _>(|a, b| a <= b),
            Instruction::I32LeU => self.relop::<u32, _>(|a, b| a <= b),
            Instruction::I32GeS => self.relop::<i32, _>(|a, b| a >= b),
            Instruction::I32GeU => self.relop::<u32, _>(|a, b| a >= b),

            Instruction::I64Eqz => self.testop::<i64, _>(|v| v == 0),
            Instruction::I64Eq => self.relop::<i64, _>(|a, b| a == b),
            Instruction::I64Ne => self.relop::<i64, _>(|a, b| a != b),
            Instruction::I64LtS => self.relop::<i64, _>(|a, b| a < b),
            Instruction::I64LtU => self.relop::<u64, _>(|a, b| a < b),
            Instruction::I64GtS => self.relop::<i64, _>(|a, b| a > b),
            Instruction::I64GtU => self.relop::<u64, _>(|a, b| a > b),
            Instruction::I64LeS => self.relop::<i64, _>(|a, b| a <= b),
            Instruction::I64LeU => self.relop::<u64, _>(|a, b| a <= b),
            Instruction::I64GeS => self.relop::<i64, _>(|a, b| a >= b),
            Instruction::I64GeU => self.relop::<u64, _>(|a, b| a >= b),

            Instruction::F32Eq => self.relop::<f32, _>(|a, b| a == b),
            Instruction::F32Ne => self.relop::<f32, _>(|a, b| a == b),
            Instruction::F32Lt => self.relop::<f32, _>(|a, b| a < b),
            Instruction::F32Gt => self.relop::<f32, _>(|a, b| a > b),
            Instruction::F32Le => self.relop::<f32, _>(|a, b| a <= b),
            Instruction::F32Ge => self.relop::<f32, _>(|a, b| a >= b),

            Instruction::F64Eq => self.relop::<f64, _>(|a, b| a == b),
            Instruction::F64Ne => self.relop::<f64, _>(|a, b| a == b),
            Instruction::F64Lt => self.relop::<f64, _>(|a, b| a < b),
            Instruction::F64Gt => self.relop::<f64, _>(|a, b| a > b),
            Instruction::F64Le => self.relop::<f64, _>(|a, b| a <= b),
            Instruction::F64Ge => self.relop::<f64, _>(|a, b| a >= b),

            Instruction::I32Clz => self.unop(|v: i32| Value::I32(v.leading_zeros() as i32)),
            Instruction::I32Ctz => self.unop(|v: i32| Value::I32(v.trailing_zeros() as i32)),
            Instruction::I32Popcnt => self.unop(|v: i32| Value::I32(v.count_ones() as i32)),
            Instruction::I32Add => self.binop (|a: i32, b: i32| Value::I32(a + b)),
            Instruction::I32Sub => self.binop (|a: i32, b: i32| Value::I32(a - b)),
            Instruction::I32Mul => self.binop (|a: i32, b: i32| Value::I32(a * b)),
            Instruction::I32DivS => self.binop(|a: i32, b: i32| Value::I32(a / b)),
            Instruction::I32DivU => {
                self.binop::<i32, _>(|a, b| Value::I32(((a / b) as u32) as i32))
            }
            Instruction::I32RemS => self.binop::<i32, _>(|a, b| Value::I32(a.wrapping_rem(b))),
            Instruction::I32RemU => {
                self.binop::<i32, _>(|a, b| Value::I32(((a.wrapping_rem(b)) as u32) as i32))
            }
            Instruction::I32And => self.binop::<i32, _>(|a, b| Value::I32(a.bitand(b))),
            Instruction::I32Or => self.binop::<i32, _>(|a, b| Value::I32(a.bitor(b))),
            Instruction::I32Xor => self.binop::<i32, _>(|a, b| Value::I32(a.bitxor(b))),
            Instruction::I32Shl => self.binop::<i32, _>(|a, b| Value::I32(a.shl(b))),
            Instruction::I32ShrS => self.binop::<i32, _>(|a, b| Value::I32(a.shr(b))),
            Instruction::I32ShrU => {
                self.binop::<i32, _>(|a, b| Value::I32((a.shr(b) as u32) as i32))
            }
            Instruction::I32Rotl => {
                self.binop::<i32, _>(|a, b| Value::I32(a.rotate_left(b as u32)))
            }
            Instruction::I32Rotr => {
                self.binop::<i32, _>(|a, b| Value::I32(a.rotate_right(b as u32)))
            }

            Instruction::I64Clz => self.unop(|v: i64| v.leading_zeros() as i64),
            Instruction::I64Ctz => self.unop(|v: i64| v.trailing_zeros() as i64),
            Instruction::I64Popcnt => self.unop(|v: i64| Value::I64(v.count_ones() as i64)),
            Instruction::I64Add => self.binop::<i64, _>(|a, b| Value::I64(a + b)),
            Instruction::I64Sub => self.binop::<i64, _>(|a, b| Value::I64(a - b)),
            Instruction::I64Mul => self.binop::<i64, _>(|a, b| Value::I64(a.wrapping_mul(b))),
            Instruction::I64DivS => self.binop::<i64, _>(|a, b| Value::I64(a / b)),
            Instruction::I64DivU => {
                self.binop::<i64, _>(|a, b| Value::I64(((a / b) as u64) as i64))
            }
            Instruction::I64RemS => self.binop::<i64, _>(|a, b| Value::I64(a.wrapping_rem(b))),
            Instruction::I64RemU => {
                self.binop::<i64, _>(|a, b| Value::I64(((a.wrapping_rem(b)) as u64) as i64))
            }
            Instruction::I64And => self.binop::<i64, _>(|a, b| Value::I64(a.bitand(b))),
            Instruction::I64Or => self.binop::<i64, _>(|a, b| Value::I64(a.bitor(b))),
            Instruction::I64Xor => self.binop::<i64, _>(|a, b| Value::I64(a.bitxor(b))),
            Instruction::I64Shl => self.binop::<i64, _>(|a, b| Value::I64(a.shl(b))),
            Instruction::I64ShrS => self.binop::<i64, _>(|a, b| Value::I64(a.shr(b))),
            Instruction::I64ShrU => {
                self.binop::<i64, _>(|a, b| Value::I64((a.shr(b) as u64) as i64))
            }
            Instruction::I64Rotl => {
                self.binop::<i64, _>(|a, b| Value::I64(a.rotate_left(b as u32)))
            }
            Instruction::I64Rotr => {
                self.binop::<i64, _>(|a, b| Value::I64(a.rotate_right(b as u32)))
            }

            Instruction::F32Abs => self.unop(|v: f32| v.abs()),
            Instruction::F32Neg => self.unop(|v: f32| -v),
            Instruction::F32Ceil => self.unop(|v: f32|  v.ceil()),
            Instruction::F32Floor => self.unop(|v: f32| v.floor()),
            Instruction::F32Trunc => self.unop(|v: f32| v.trunc()),
            Instruction::F32Nearest => self.unop(|v: f32| v.round()),
            Instruction::F32Sqrt => self.unop(|v: f32| Value::F32(v.sqrt())),
            Instruction::F32Add => self.binop(|a: f32, b: f32| Value::F32(a + b)),
            Instruction::F32Sub => self.binop(|a: f32, b: f32| Value::F32(a - b)),
            Instruction::F32Mul => self.binop(|a: f32, b: f32| Value::F32(a * b)),
            Instruction::F32Div => self.binop(|a: f32, b: f32| Value::F32(a / b)),
            Instruction::F32Min => self.binop(|a: f32, b: f32| Value::F32(a.min(b))),
            Instruction::F32Max => self.binop(|a: f32, b: f32| Value::F32(a.max(b))),
            Instruction::F32Copysign => unimplemented!(),

            Instruction::F64Abs => self.unop(|v: f64| Value::F64(v.abs())),
            Instruction::F64Neg => self.unop(|v: f64| Value::F64(-v)),
            Instruction::F64Ceil => self.unop(|v: f64| Value::F64(v.ceil())),
            Instruction::F64Floor => self.unop(|v: f64| Value::F64(v.floor())),
            Instruction::F64Trunc => self.unop(|v: f64| Value::F64(v.trunc())),
            Instruction::F64Nearest => self.unop(|v: f64| Value::F64(v.round())),
            Instruction::F64Sqrt => self.unop(|v: f64| Value::F64(v.sqrt())),
            Instruction::F64Add => self.binop::<f64, _>(|a, b| Value::F64(a + b)),
            Instruction::F64Sub => self.binop::<f64, _>(|a, b| Value::F64(a - b)),
            Instruction::F64Mul => self.binop::<f64, _>(|a, b| Value::F64(a * b)),
            Instruction::F64Div => self.binop::<f64, _>(|a, b| Value::F64(a / b)),
            Instruction::F64Min => self.binop::<f64, _>(|a, b| Value::F64(a.min(b))),
            Instruction::F64Max => self.binop::<f64, _>(|a, b| Value::F64(a.max(b))),
            Instruction::F64Copysign => unimplemented!(),

            Instruction::I32WrapI64 => {
                self.unop(|v: i32| Value::I64((f64::from(v) as i32).into()))
            }
            Instruction::I32TruncSF32 => self.unop(|v: f32| v as i64),
            Instruction::I32TruncUF32 => self.unop(|v: f32| (v as f32).trunc()),
            Instruction::I32TruncSF64 => self.unop(|v: f64| v as f64),
            Instruction::I32TruncUF64 => self.unop(|v: f64| v as f64),
            Instruction::I64ExtendSI32 => self.unop(|v: i32| Value::I64(v as i64)),
            Instruction::I64ExtendUI32 => self.unop(|v: i32| Value::I64((v as u64) as i64)),
            Instruction::I64TruncSF32 => unimplemented!(),
            Instruction::I64TruncUF32 => unimplemented!(),
            Instruction::I64TruncSF64 => unimplemented!(),
            Instruction::I64TruncUF64 => unimplemented!(),
            Instruction::F32ConvertSI32 => unimplemented!(),
            Instruction::F32ConvertUI32 => unimplemented!(),
            Instruction::F32ConvertSI64 => unimplemented!(),
            Instruction::F32ConvertUI64 => unimplemented!(),
            Instruction::F32DemoteF64 => unimplemented!(),
            Instruction::F64ConvertSI32 => unimplemented!(),
            Instruction::F64ConvertUI32 => unimplemented!(),
            Instruction::F64ConvertSI64 => unimplemented!(),
            Instruction::F64ConvertUI64 => unimplemented!(),
            Instruction::F64PromoteF32 => unimplemented!(),

            Instruction::I32ReinterpretF32 => self.unop(|v: f32| v.to_bits() as i32),
            Instruction::I64ReinterpretF64 => self.unop(|v: f64| v.to_bits() as i64),
            Instruction::F32ReinterpretI32 => unimplemented!(),
            Instruction::F64ReinterpretI64 => unimplemented!(),
        };
        if self.stack.is_over_top_level() {
            return Ok(ExecSuccess::End);
        } else {
            return result;
        }
    }

    fn pop_as<T: NativeValue>(&mut self) -> T {
        let value = self.stack.pop_value();
        match T::from_value(value) {
            Some(val) => val,
            None => panic!(),
        }
    }

    fn branch(&mut self, depth: u32) -> ExecResult {
        let depth = depth as usize;
        let label = {
            let labels = self.stack.current_frame_labels();
            let labels_len = labels.len();
            assert!(depth + 1 <= labels_len);
            *labels[labels_len - depth - 1]
        };

        let arity = label.arity();

        let mut results = vec![];
        for _ in 0..arity {
            results.push(self.stack.pop_value());
        }

        for _ in 0..depth + 1 {
            self.stack.pop_while(|v| match v {
                StackValue::Value(_) => true,
                _ => false,
            });
            self.stack.pop_label();
        }

        for _ in 0..arity {
            self.stack.push_value(results.pop().unwrap());
        }

        // Jump to the continuation
        println!("> Jump to the continuation");
        match label {
            Label::Loop(loop_label) => self.pc.loop_jump(&loop_label),
            Label::Return(_) => {
                return self.do_return();
            }
            Label::If(_) | Label::Block(_) => {
                let mut depth = depth + 1;
                loop {
                    let index = self.pc.inst_index().0 as usize;
                    match self.current_func_insts()[index] {
                        Instruction::End => depth -= 1,
                        Instruction::Block(_) => depth += 1,
                        Instruction::If(_) => depth += 1,
                        Instruction::Loop(_) => depth += 1,
                        _ => (),
                    }
                    self.pc.inc_inst_index();
                    if depth == 0 {
                        break;
                    }
                }
            }
        }
        Ok(ExecSuccess::Next)
    }

    fn testop<T: NativeValue, F: Fn(T) -> bool>(&mut self, f: F) -> ExecResult {
        self.unop(|a| Value::I32(if f(a) { 1 } else { 0 }))
    }

    fn relop<T: NativeValue, F: Fn(T, T) -> bool>(&mut self, f: F) -> ExecResult {
        self.binop::<T, _>(|a, b| Value::I32(if f(a, b) { 1 } else { 0 }))
    }

    fn binop<T: NativeValue, F: Fn(T, T) -> Value>(&mut self, f: F) -> ExecResult {
        let rhs = self.pop_as();
        let lhs = self.pop_as();
        self.stack.push_value(f(lhs, rhs));
        Ok(ExecSuccess::Next)
    }

    fn unop<From: NativeValue, To: Into<Value>, F: Fn(From) -> To>(&mut self, f: F) -> ExecResult {
        let v: From = self.pop_as();
        self.stack.push_value(f(v).into());
        Ok(ExecSuccess::Next)
    }

    fn invoke(&mut self, addr: FuncAddr) -> ExecResult {
        let func = self.store.func(addr);
        let arity = func.ty().return_type().map(|_| 1).unwrap_or(0);
        println!("--- Start of Function {:?} ---", func.ty());

        // println!("{:?}", self.stack);
        let mut args = Vec::new();
        for _ in func.ty().params() {
            args.push(self.stack.pop_value());
        }
        match func {
            FunctionInstance::Defined(defined) => {
                let pc = ProgramCounter::new(addr, InstIndex::zero());
                args.reverse();
                let frame = CallFrame::new_from_func(addr, &defined, args, Some(self.pc));
                self.stack.set_frame(frame);
                self.stack.push_label(Label::Return(arity));
                self.pc = pc;
                Ok(ExecSuccess::Next)
            }
            FunctionInstance::Host(host) => match &host.field_name()[..] {
                "print_i32" => {
                    BuiltinPrintI32::dispatch(&args);
                    Ok(ExecSuccess::Next)
                }
                _ => panic!(),
            },
        }
    }
    fn do_return(&mut self) -> ExecResult {
        let frame = self.stack.current_frame().clone();
        let func = self.store.func(frame.func_addr);
        println!("--- Function return {:?} ---", func.ty());
        let arity = func.ty().return_type().map(|_| 1).unwrap_or(0);
        let mut result = vec![];
        for _ in 0..arity {
            result.push(self.stack.pop_value());
        }
        self.stack.pop_while(|v| match v {
            StackValue::Activation(_) => false,
            _ => true,
        });
        self.stack.pop_frame();
        for v in result {
            self.stack.push_value(v);
        }

        if let Some(ret_pc) = frame.ret_pc {
            self.pc = ret_pc;
        }
        Ok(ExecSuccess::Next)
    }

    fn set_local(&mut self, index: usize) -> ExecResult {
        let value = self.stack.pop_value();
        self.stack.set_local(index, value);

        Ok(ExecSuccess::Next)
    }

    fn store<T: NativeValue + IntoLittleEndian>(&mut self, offset: usize) -> ExecResult {
        let val: T = self.pop_as();
        let raw_addr: i32 = self.pop_as();
        let raw_addr = raw_addr as usize;
        let addr: usize = raw_addr + offset;
        let frame = self.stack.current_frame();
        let mem_addr = MemoryAddr(frame.module_index(), 0);
        let memory = { self.store.memory(mem_addr) };
        let mem_len = match memory {
            MemoryInstance::Defined(memory) => memory.data_len(),
            MemoryInstance::External(_) => panic!(),
        };
        let elem_size = std::mem::size_of::<T>();
        if (addr + elem_size) > mem_len {
            panic!();
        }
        let mut buf: Vec<u8> = std::iter::repeat(0).take(elem_size).collect();
        val.into_le(&mut buf);
        self.store.memory_mut(mem_addr).initialize(addr, &buf);
        Ok(ExecSuccess::Next)
    }

    fn store_with_width<T: NativeValue + IntoLittleEndian>(
        &mut self,
        offset: usize,
        width: usize,
    ) -> ExecResult {
        let val: T = self.pop_as();
        let raw_addr: i32 = self.pop_as();
        let raw_addr = raw_addr as usize;
        let addr: usize = raw_addr + offset;
        let frame = self.stack.current_frame();
        let mem_addr = MemoryAddr(frame.module_index(), 0);
        let memory = { self.store.memory(mem_addr) };
        let mem_len = memory.data_len();
        let elem_size = width;
        if (addr + elem_size) > mem_len {
            panic!();
        }
        let mut buf: Vec<u8> = std::iter::repeat(0)
            .take(std::mem::size_of::<T>())
            .collect();
        val.into_le(&mut buf);
        self.store.memory_mut(mem_addr).initialize(addr, &buf);
        Ok(ExecSuccess::Next)
    }

    fn load<T>(&mut self, offset: usize) -> ExecResult
    where
        T: NativeValue + FromLittleEndian,
        T: Into<Value>,
    {
        let raw_addr: i32 = self.pop_as();
        let raw_addr = raw_addr as usize;
        let addr: usize = raw_addr + offset;

        let frame = self.stack.current_frame();
        let mem_addr = MemoryAddr(frame.module_index(), 0);
        let memory = { self.store.memory(mem_addr) };
        let mem_len = memory.data_len();
        let elem_size = std::mem::size_of::<T>();
        if (addr + elem_size) > mem_len {
            panic!();
        }
        let result: T = memory.load_as(addr);
        self.stack.push_value(result.into());
        Ok(ExecSuccess::Next)
    }

    fn load_extend<T: FromLittleEndian + ExtendInto<U>, U: Into<Value>>(
        &mut self,
        offset: usize,
    ) -> ExecResult {
        let raw_addr: i32 = self.pop_as();
        let raw_addr = raw_addr as usize;
        let addr: usize = raw_addr + offset;

        let frame = self.stack.current_frame();
        let mem_addr = MemoryAddr(frame.module_index(), 0);
        let memory = { self.store.memory(mem_addr) };
        let mem_len = memory.data_len();
        let elem_size = std::mem::size_of::<T>();
        if (addr + elem_size) > mem_len {
            panic!();
        }
        let result: T = memory.load_as(addr);
        let result = result.extend_into();
        self.stack.push_value(result.into());
        Ok(ExecSuccess::Next)
    }

    fn reinterpret<From, To>(&mut self) -> ExecResult {
        // let v = self.stack.pop_
        panic!()
    }
}

pub fn eval_const_expr(init_expr: &InitExpr, store: &Store, module_index: ModuleIndex) -> Value {
    let inst = &init_expr.code()[0];
    match *inst {
        Instruction::I32Const(val) => Value::I32(val),
        Instruction::I64Const(val) => Value::I64(val),
        Instruction::F32Const(val) => Value::F32(f32::from_bits(val)),
        Instruction::F64Const(val) => Value::F64(f64::from_bits(val)),
        Instruction::GetGlobal(index) => {
            let addr = GlobalAddr(module_index, index as usize);
            store.global(addr).value(store)
        }
        _ => panic!("Unsupported init_expr {}", inst),
    }
}
