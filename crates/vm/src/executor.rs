use super::address::{FuncAddr, GlobalAddr, MemoryAddr, TableAddr};
use super::func::*;
use super::inst::{Instruction, InstructionKind};
use super::interceptor::{Interceptor, NopInterceptor};
use super::memory;
use super::memory::MemoryInstance;
use super::module::*;
use super::stack;
use super::stack::{CallFrame, Label, ProgramCounter, Stack, StackValue};
use super::store::*;
use super::table;
use super::value;
use super::value::{
    ExtendInto, FromLittleEndian, IntoLittleEndian, NativeValue, Value, F32, F64, I32, I64, U32,
    U64,
};
use wasmparser::{FuncType, Type, TypeOrFuncType};

use std::ops::*;

#[derive(Debug)]
pub enum Trap {
    Unreachable,
    Memory(memory::Error),
    Stack(stack::Error),
    Table(table::Error),
    Value(value::Error),
    IndirectCallTypeMismatch(
        String,
        /* expected: */ FuncType,
        /* actual: */ FuncType,
    ),
    DirectCallTypeMismatch {
        func_name: String,
        expected: Vec<Type>,
        actual: Vec<Type>,
    },
    UnexpectedStackValueType(/* expected: */ Type, /* actual: */ Type),
    UndefinedFunc(usize),
}

impl std::error::Error for Trap {}

impl std::fmt::Display for Trap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory(e) => write!(f, "{}", e),
            Self::Value(e) => write!(f, "{}", e),
            Self::Table(e) => write!(f, "{}", e),
            Self::Stack(e) => write!(f, "{}", e),
            Self::IndirectCallTypeMismatch(name, expected, actual) => write!(
                f,
                "indirect call type mismatch, expected {:?} but got {:?} '{}'",
                expected, actual, name
            ),
            Self::UndefinedFunc(addr) => write!(f, "uninitialized func at {:?}", addr),
            Self::Unreachable => write!(f, "unreachable"),
            _ => write!(f, "{:?}", self),
        }
    }
}

pub enum Signal {
    Next,
    Breakpoint,
    End,
}

pub type ExecResult<T> = std::result::Result<T, Trap>;

#[derive(Debug)]
pub enum ReturnValError {
    TypeMismatchReturnValue(Value, Type),
    Stack(stack::Error),
    NoValue(Type),
}

pub type ReturnValResult = Result<Vec<Value>, ReturnValError>;

pub struct Executor {
    pub pc: ProgramCounter,
    pub stack: Stack,
}

impl Executor {
    pub fn new(initial_frame: CallFrame, initial_arity: usize, pc: ProgramCounter) -> Self {
        let mut stack = Stack::default();
        let _ = stack.set_frame(initial_frame);
        stack.push_label(Label::Return(initial_arity));
        Self { pc, stack }
    }

    pub fn pop_result(&mut self, return_ty: Vec<Type>) -> ReturnValResult {
        let mut results = vec![];
        for ty in return_ty {
            let val = self.stack.pop_value().map_err(ReturnValError::Stack)?;
            results.push(val);
            if val.value_type() != ty {
                return Err(ReturnValError::TypeMismatchReturnValue(val.clone(), ty));
            }
        }
        Ok(results)
    }

    pub fn current_func_insts<'a>(&self, store: &'a Store) -> ExecResult<&'a [Instruction]> {
        let func = store.func_global(self.pc.exec_addr());
        Ok(&func.defined().unwrap().instructions())
    }

    pub fn execute_step<I: Interceptor>(
        &mut self,
        store: &Store,
        interceptor: &I,
    ) -> ExecResult<Signal> {
        let func = store.func_global(self.pc.exec_addr()).defined().unwrap();
        let module_index = func.module_index().clone();
        let inst = func.inst(self.pc.inst_index());
        return self.execute_inst(&inst, module_index, store, interceptor);
    }

    fn execute_inst<I: Interceptor>(
        &mut self,
        inst: &Instruction,
        module_index: ModuleIndex,
        store: &Store,
        interceptor: &I,
    ) -> ExecResult<Signal> {
        self.pc.inc_inst_index();
        // println!("{:?}", self.stack);
        // {
        //     let mut indent = String::new();
        //     for _ in 0..self
        //         .stack
        //         .current_frame_labels()
        //         .map_err(Trap::Stack)?
        //         .len()
        //     {
        //         indent.push_str("  ");
        //     }
        //     println!("{}{}", indent, inst.clone());
        // }
        let result = match inst.kind.clone() {
            InstructionKind::Unreachable => Err(Trap::Unreachable),
            InstructionKind::Nop => Ok(Signal::Next),
            InstructionKind::Block { ty } => {
                self.stack.push_label(Label::Block({
                    match ty {
                        TypeOrFuncType::Type(Type::EmptyBlockType) => 0,
                        TypeOrFuncType::Type(_) => 1,
                        TypeOrFuncType::FuncType(_) => 1,
                    }
                }));
                Ok(Signal::Next)
            }
            InstructionKind::Loop { ty: _ } => {
                let start_loop = InstIndex(self.pc.inst_index().0 - 1);
                self.stack.push_label(Label::new_loop(start_loop));
                Ok(Signal::Next)
            }
            InstructionKind::If { ty } => {
                let val: i32 = self.pop_as()?;
                self.stack.push_label(Label::If(match ty {
                    TypeOrFuncType::Type(Type::EmptyBlockType) => 0,
                    TypeOrFuncType::Type(_) => 1,
                    TypeOrFuncType::FuncType(_) => 1,
                }));
                if val == 0 {
                    let mut depth = 1;
                    loop {
                        let index = self.pc.inst_index().0 as usize;
                        match self.current_func_insts(store)?[index].kind {
                            InstructionKind::End => depth -= 1,
                            InstructionKind::Block { ty: _ } => depth += 1,
                            InstructionKind::If { ty: _ } => depth += 1,
                            InstructionKind::Loop { ty: _ } => depth += 1,
                            InstructionKind::Else => {
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
                Ok(Signal::Next)
            }
            InstructionKind::Else => self.branch(0, store),
            InstructionKind::End => {
                if self.stack.is_func_top_level().map_err(Trap::Stack)? {
                    // When the end of a function is reached without a jump
                    let ret_pc = self.stack.current_frame().map_err(Trap::Stack)?.ret_pc;
                    let func = store.func_global(self.pc.exec_addr());
                    let arity = func.ty().returns.len();
                    let mut result = vec![];
                    for _ in 0..arity {
                        result.push(self.stack.pop_value().map_err(Trap::Stack)?);
                    }
                    self.stack.pop_label().map_err(Trap::Stack)?;
                    self.stack.pop_frame().map_err(Trap::Stack)?;
                    for v in result {
                        self.stack.push_value(v);
                    }
                    if let Some(ret_pc) = ret_pc {
                        self.pc = ret_pc;
                        Ok(Signal::Next)
                    } else {
                        Ok(Signal::End)
                    }
                } else {
                    // When the end of a block is reached without a jump
                    let results = self.stack.pop_while(|v| match v {
                        StackValue::Value(_) => true,
                        _ => false,
                    });
                    self.stack.pop_label().map_err(Trap::Stack)?;
                    for v in results {
                        self.stack.push_value(v.as_value().map_err(Trap::Stack)?);
                    }
                    Ok(Signal::Next)
                }
            }
            InstructionKind::Br { relative_depth } => self.branch(relative_depth, store),
            InstructionKind::BrIf { relative_depth } => {
                let val = self.stack.pop_value().map_err(Trap::Stack)?;
                if val != Value::I32(0) {
                    self.branch(relative_depth, store)
                } else {
                    Ok(Signal::Next)
                }
            }
            InstructionKind::BrTable { table: payload } => {
                let val: i32 = self.pop_as()?;
                let val = val as usize;
                let depth = if val < payload.table.len() {
                    payload.table[val]
                } else {
                    payload.default
                };
                self.branch(depth, store)
            }
            InstructionKind::Return => self.do_return(store),
            InstructionKind::Call { function_index } => {
                let frame = self.stack.current_frame().map_err(Trap::Stack)?;
                let addr = FuncAddr::new_unsafe(frame.module_index(), function_index as usize);
                self.invoke(addr, store, interceptor)
            }
            InstructionKind::CallIndirect {
                index,
                table_index: _,
            } => {
                let frame = self.stack.current_frame().map_err(Trap::Stack)?;
                let addr = TableAddr::new_unsafe(frame.module_index(), 0);
                let module = store.module(frame.module_index()).defined().unwrap();
                let ty = module.get_type(index as usize);
                let buf_index: i32 = self.pop_as()?;
                let table = store.table(addr);
                let buf_index = buf_index as usize;
                let func_addr = table.borrow().get_at(buf_index).map_err(Trap::Table)?;
                let (func, _) = store
                    .func(func_addr)
                    .ok_or(Trap::UndefinedFunc(func_addr.1))?;
                if eq_func_type(func.ty(), &ty) {
                    self.invoke(func_addr, store, interceptor)
                } else {
                    Err(Trap::IndirectCallTypeMismatch(
                        func.name().clone(),
                        ty.clone(),
                        func.ty().clone(),
                    ))
                }
            }
            InstructionKind::Drop => {
                self.stack.pop_value().map_err(Trap::Stack)?;
                Ok(Signal::Next)
            }
            InstructionKind::Select => {
                let cond: i32 = self.pop_as()?;
                let val2 = self.stack.pop_value().map_err(Trap::Stack)?;
                let val1 = self.stack.pop_value().map_err(Trap::Stack)?;
                if cond != 0 {
                    self.stack.push_value(val1);
                } else {
                    self.stack.push_value(val2);
                }
                Ok(Signal::Next)
            }
            InstructionKind::LocalGet { local_index } => {
                let value = self
                    .stack
                    .current_frame()
                    .map_err(Trap::Stack)?
                    .local(local_index as usize);
                self.stack.push_value(value);
                Ok(Signal::Next)
            }
            InstructionKind::LocalSet { local_index } => self.set_local(local_index as usize),
            InstructionKind::LocalTee { local_index } => {
                let val = self.stack.pop_value().map_err(Trap::Stack)?;
                self.stack.push_value(val);
                self.stack.push_value(val);
                self.set_local(local_index as usize)
            }
            InstructionKind::GlobalGet { global_index } => {
                let addr = GlobalAddr::new_unsafe(module_index, global_index as usize);
                let global = store.global(addr);
                self.stack.push_value(global.borrow().value());
                Ok(Signal::Next)
            }
            InstructionKind::GlobalSet { global_index } => {
                let addr = GlobalAddr::new_unsafe(module_index, global_index as usize);
                let value = self.stack.pop_value().map_err(Trap::Stack)?;
                let global = store.global(addr);
                global.borrow_mut().set_value(value);
                Ok(Signal::Next)
            }

            InstructionKind::I32Load { memarg } => self.load::<i32>(memarg.offset as usize, store),
            InstructionKind::I64Load { memarg } => self.load::<i64>(memarg.offset as usize, store),
            InstructionKind::F32Load { memarg } => self.load::<f32>(memarg.offset as usize, store),
            InstructionKind::F64Load { memarg } => self.load::<f64>(memarg.offset as usize, store),

            InstructionKind::I32Load8S { memarg } => {
                self.load_extend::<i8, i32>(memarg.offset as usize, store)
            }
            InstructionKind::I32Load8U { memarg } => {
                self.load_extend::<u8, i32>(memarg.offset as usize, store)
            }
            InstructionKind::I32Load16S { memarg } => {
                self.load_extend::<i16, i32>(memarg.offset as usize, store)
            }
            InstructionKind::I32Load16U { memarg } => {
                self.load_extend::<u16, i32>(memarg.offset as usize, store)
            }

            InstructionKind::I64Load8S { memarg } => {
                self.load_extend::<i8, i64>(memarg.offset as usize, store)
            }
            InstructionKind::I64Load8U { memarg } => {
                self.load_extend::<u8, i64>(memarg.offset as usize, store)
            }
            InstructionKind::I64Load16S { memarg } => {
                self.load_extend::<i16, i64>(memarg.offset as usize, store)
            }
            InstructionKind::I64Load16U { memarg } => {
                self.load_extend::<u16, i64>(memarg.offset as usize, store)
            }
            InstructionKind::I64Load32S { memarg } => {
                self.load_extend::<i32, i64>(memarg.offset as usize, store)
            }
            InstructionKind::I64Load32U { memarg } => {
                self.load_extend::<u32, i64>(memarg.offset as usize, store)
            }

            InstructionKind::I32Store { memarg } => {
                self.store::<i32>(memarg.offset as usize, store)
            }
            InstructionKind::I64Store { memarg } => {
                self.store::<i64>(memarg.offset as usize, store)
            }
            InstructionKind::F32Store { memarg } => {
                self.store::<f32>(memarg.offset as usize, store)
            }
            InstructionKind::F64Store { memarg } => {
                self.store::<f64>(memarg.offset as usize, store)
            }

            InstructionKind::I32Store8 { memarg } => {
                self.store_with_width::<i32>(memarg.offset as usize, 1, store)
            }
            InstructionKind::I32Store16 { memarg } => {
                self.store_with_width::<i32>(memarg.offset as usize, 2, store)
            }
            InstructionKind::I64Store8 { memarg } => {
                self.store_with_width::<i64>(memarg.offset as usize, 1, store)
            }
            InstructionKind::I64Store16 { memarg } => {
                self.store_with_width::<i64>(memarg.offset as usize, 2, store)
            }
            InstructionKind::I64Store32 { memarg } => {
                self.store_with_width::<i64>(memarg.offset as usize, 4, store)
            }

            InstructionKind::MemorySize { reserved: _ } => {
                self.stack
                    .push_value(Value::I32(self.memory(store)?.borrow().page_count() as i32));
                Ok(Signal::Next)
            }
            InstructionKind::MemoryGrow { reserved: _ } => {
                let grow_page: i32 = self.pop_as()?;
                let mem = self.memory(store)?;
                let size = mem.borrow().page_count();
                match mem.borrow_mut().grow(grow_page as usize) {
                    Ok(_) => {
                        self.stack.push_value(Value::I32(size as i32));
                    }
                    Err(err) => {
                        println!("[Debug] Failed to grow memory {:?}", err);
                        self.stack.push_value(Value::I32(-1));
                    }
                }
                Ok(Signal::Next)
            }

            InstructionKind::I32Const { value } => {
                self.stack.push_value(Value::I32(value));
                Ok(Signal::Next)
            }
            InstructionKind::I64Const { value } => {
                self.stack.push_value(Value::I64(value));
                Ok(Signal::Next)
            }
            InstructionKind::F32Const { value } => {
                self.stack.push_value(Value::F32(value.bits()));
                Ok(Signal::Next)
            }
            InstructionKind::F64Const { value } => {
                self.stack.push_value(Value::F64(value.bits()));
                Ok(Signal::Next)
            }

            InstructionKind::I32Eqz => self.testop::<i32, _>(|v| v == 0),
            InstructionKind::I32Eq => self.relop(|a: i32, b: i32| a == b),
            InstructionKind::I32Ne => self.relop(|a: i32, b: i32| a != b),
            InstructionKind::I32LtS => self.relop(|a: i32, b: i32| a < b),
            InstructionKind::I32LtU => self.relop::<u32, _>(|a, b| a < b),
            InstructionKind::I32GtS => self.relop(|a: i32, b: i32| a > b),
            InstructionKind::I32GtU => self.relop::<u32, _>(|a, b| a > b),
            InstructionKind::I32LeS => self.relop(|a: i32, b: i32| a <= b),
            InstructionKind::I32LeU => self.relop::<u32, _>(|a, b| a <= b),
            InstructionKind::I32GeS => self.relop(|a: i32, b: i32| a >= b),
            InstructionKind::I32GeU => self.relop::<u32, _>(|a, b| a >= b),

            InstructionKind::I64Eqz => self.testop::<i64, _>(|v| v == 0),
            InstructionKind::I64Eq => self.relop(|a: i64, b: i64| a == b),
            InstructionKind::I64Ne => self.relop(|a: i64, b: i64| a != b),
            InstructionKind::I64LtS => self.relop(|a: i64, b: i64| a < b),
            InstructionKind::I64LtU => self.relop::<u64, _>(|a, b| a < b),
            InstructionKind::I64GtS => self.relop(|a: i64, b: i64| a > b),
            InstructionKind::I64GtU => self.relop::<u64, _>(|a, b| a > b),
            InstructionKind::I64LeS => self.relop(|a: i64, b: i64| a <= b),
            InstructionKind::I64LeU => self.relop::<u64, _>(|a, b| a <= b),
            InstructionKind::I64GeS => self.relop(|a: i64, b: i64| a >= b),
            InstructionKind::I64GeU => self.relop::<u64, _>(|a, b| a >= b),

            InstructionKind::F32Eq => self.relop::<f32, _>(|a, b| a == b),
            InstructionKind::F32Ne => self.relop::<f32, _>(|a, b| a != b),
            InstructionKind::F32Lt => self.relop::<f32, _>(|a, b| a < b),
            InstructionKind::F32Gt => self.relop::<f32, _>(|a, b| a > b),
            InstructionKind::F32Le => self.relop::<f32, _>(|a, b| a <= b),
            InstructionKind::F32Ge => self.relop::<f32, _>(|a, b| a >= b),

            InstructionKind::F64Eq => self.relop(|a: f64, b: f64| a == b),
            InstructionKind::F64Ne => self.relop(|a: f64, b: f64| a != b),
            InstructionKind::F64Lt => self.relop(|a: f64, b: f64| a < b),
            InstructionKind::F64Gt => self.relop(|a: f64, b: f64| a > b),
            InstructionKind::F64Le => self.relop(|a: f64, b: f64| a <= b),
            InstructionKind::F64Ge => self.relop(|a: f64, b: f64| a >= b),

            InstructionKind::I32Clz => self.unop(|v: i32| v.leading_zeros() as i32),
            InstructionKind::I32Ctz => self.unop(|v: i32| v.trailing_zeros() as i32),
            InstructionKind::I32Popcnt => self.unop(|v: i32| v.count_ones() as i32),
            InstructionKind::I32Add => self.binop(|a: u32, b: u32| a.wrapping_add(b)),
            InstructionKind::I32Sub => self.binop(|a: i32, b: i32| a.wrapping_sub(b)),
            InstructionKind::I32Mul => self.binop(|a: i32, b: i32| a.wrapping_mul(b)),
            InstructionKind::I32DivS => {
                self.try_binop(|a: i32, b: i32| I32::try_wrapping_div(a, b))
            }
            InstructionKind::I32DivU => {
                self.try_binop(|a: u32, b: u32| U32::try_wrapping_div(a, b))
            }
            InstructionKind::I32RemS => {
                self.try_binop(|a: i32, b: i32| I32::try_wrapping_rem(a, b))
            }
            InstructionKind::I32RemU => {
                self.try_binop(|a: u32, b: u32| U32::try_wrapping_rem(a, b))
            }
            InstructionKind::I32And => self.binop(|a: i32, b: i32| a.bitand(b)),
            InstructionKind::I32Or => self.binop(|a: i32, b: i32| a.bitor(b)),
            InstructionKind::I32Xor => self.binop(|a: i32, b: i32| a.bitxor(b)),
            InstructionKind::I32Shl => self.binop(|a: u32, b: u32| a.wrapping_shl(b)),
            InstructionKind::I32ShrS => self.binop(|a: i32, b: i32| a.wrapping_shr(b as u32)),
            InstructionKind::I32ShrU => self.binop(|a: u32, b: u32| a.wrapping_shr(b)),
            InstructionKind::I32Rotl => self.binop(|a: i32, b: i32| a.rotate_left(b as u32)),
            InstructionKind::I32Rotr => self.binop(|a: i32, b: i32| a.rotate_right(b as u32)),

            InstructionKind::I64Clz => self.unop(|v: i64| v.leading_zeros() as i64),
            InstructionKind::I64Ctz => self.unop(|v: i64| v.trailing_zeros() as i64),
            InstructionKind::I64Popcnt => self.unop(|v: i64| v.count_ones() as i64),
            InstructionKind::I64Add => self.binop(|a: i64, b: i64| a.wrapping_add(b)),
            InstructionKind::I64Sub => self.binop(|a: i64, b: i64| a.wrapping_sub(b)),
            InstructionKind::I64Mul => self.binop(|a: i64, b: i64| a.wrapping_mul(b)),
            InstructionKind::I64DivS => {
                self.try_binop(|a: i64, b: i64| I64::try_wrapping_div(a, b))
            }
            InstructionKind::I64DivU => {
                self.try_binop(|a: u64, b: u64| U64::try_wrapping_div(a, b))
            }
            InstructionKind::I64RemS => {
                self.try_binop(|a: i64, b: i64| I64::try_wrapping_rem(a, b))
            }
            InstructionKind::I64RemU => {
                self.try_binop(|a: u64, b: u64| U64::try_wrapping_rem(a, b))
            }
            InstructionKind::I64And => self.binop(|a: i64, b: i64| a.bitand(b)),
            InstructionKind::I64Or => self.binop(|a: i64, b: i64| a.bitor(b)),
            InstructionKind::I64Xor => self.binop(|a: i64, b: i64| a.bitxor(b)),
            InstructionKind::I64Shl => self.binop(|a: u64, b: u64| a.wrapping_shl(b as u32)),
            InstructionKind::I64ShrS => self.binop(|a: i64, b: i64| a.wrapping_shr(b as u32)),
            InstructionKind::I64ShrU => self.binop(|a: u64, b: u64| a.wrapping_shr(b as u32)),
            InstructionKind::I64Rotl => self.binop(|a: i64, b: i64| a.rotate_left(b as u32)),
            InstructionKind::I64Rotr => self.binop(|a: i64, b: i64| a.rotate_right(b as u32)),

            InstructionKind::F32Abs => self.unop(|v: f32| v.abs()),
            InstructionKind::F32Neg => self.unop(|v: f32| -v),
            InstructionKind::F32Ceil => self.unop(|v: f32| v.ceil()),
            InstructionKind::F32Floor => self.unop(|v: f32| v.floor()),
            InstructionKind::F32Trunc => self.unop(|v: f32| v.trunc()),
            InstructionKind::F32Nearest => self.unop(|v: f32| F32::nearest(v)),
            InstructionKind::F32Sqrt => self.unop(|v: f32| v.sqrt()),
            InstructionKind::F32Add => self.binop(|a: f32, b: f32| a + b),
            InstructionKind::F32Sub => self.binop(|a: f32, b: f32| a - b),
            InstructionKind::F32Mul => self.binop(|a: f32, b: f32| a * b),
            InstructionKind::F32Div => self.binop(|a: f32, b: f32| a / b),
            InstructionKind::F32Min => self.binop(|a: f32, b: f32| F32::min(a, b)),
            InstructionKind::F32Max => self.binop(|a: f32, b: f32| F32::max(a, b)),
            InstructionKind::F32Copysign => self.binop(|a: f32, b: f32| F32::copysign(a, b)),

            InstructionKind::F64Abs => self.unop(|v: f64| v.abs()),
            InstructionKind::F64Neg => self.unop(|v: f64| -v),
            InstructionKind::F64Ceil => self.unop(|v: f64| v.ceil()),
            InstructionKind::F64Floor => self.unop(|v: f64| v.floor()),
            InstructionKind::F64Trunc => self.unop(|v: f64| v.trunc()),
            InstructionKind::F64Nearest => self.unop(|v: f64| F64::nearest(v)),
            InstructionKind::F64Sqrt => self.unop(|v: f64| v.sqrt()),
            InstructionKind::F64Add => self.binop(|a: f64, b: f64| a + b),
            InstructionKind::F64Sub => self.binop(|a: f64, b: f64| a - b),
            InstructionKind::F64Mul => self.binop(|a: f64, b: f64| a * b),
            InstructionKind::F64Div => self.binop(|a: f64, b: f64| a / b),
            InstructionKind::F64Min => self.binop(|a: f64, b: f64| F64::min(a, b)),
            InstructionKind::F64Max => self.binop(|a: f64, b: f64| F64::max(a, b)),
            InstructionKind::F64Copysign => self.binop(|a: f64, b: f64| F64::copysign(a, b)),

            InstructionKind::I32WrapI64 => self.unop(|v: i64| Value::I32(v as i32)),
            InstructionKind::I32TruncF32S => self.try_unop(|v: f32| F32::trunc_to_i32(v)),
            InstructionKind::I32TruncF32U => self.try_unop(|v: f32| F32::trunc_to_u32(v)),
            InstructionKind::I32TruncF64S => self.try_unop(|v: f64| F64::trunc_to_i32(v)),
            InstructionKind::I32TruncF64U => self.try_unop(|v: f64| F64::trunc_to_u32(v)),
            InstructionKind::I64ExtendI32S => self.unop(|v: i32| Value::from(v as u64)),
            InstructionKind::I64ExtendI32U => self.unop(|v: u32| Value::from(v as u64)),
            InstructionKind::I64TruncF32S => self.try_unop(|x: f32| F32::trunc_to_i64(x)),
            InstructionKind::I64TruncF32U => self.try_unop(|x: f32| F32::trunc_to_u64(x)),
            InstructionKind::I64TruncF64S => self.try_unop(|x: f64| F64::trunc_to_i64(x)),
            InstructionKind::I64TruncF64U => self.try_unop(|x: f64| F64::trunc_to_u64(x)),
            InstructionKind::F32ConvertI32S => self.unop(|x: u32| x as i32 as f32),
            InstructionKind::F32ConvertI32U => self.unop(|x: u32| x as f32),
            InstructionKind::F32ConvertI64S => self.unop(|x: u64| x as i64 as f32),
            InstructionKind::F32ConvertI64U => self.unop(|x: u64| x as f32),
            InstructionKind::F32DemoteF64 => self.unop(|x: f64| x as f32),
            InstructionKind::F64ConvertI32S => self.unop(|x: u32| f64::from(x as i32)),
            InstructionKind::F64ConvertI32U => self.unop(|x: u32| f64::from(x)),
            InstructionKind::F64ConvertI64S => self.unop(|x: u64| x as i64 as f64),
            InstructionKind::F64ConvertI64U => self.unop(|x: u64| x as f64),
            InstructionKind::F64PromoteF32 => self.unop(|x: f32| f64::from(x)),

            InstructionKind::I32ReinterpretF32 => self.unop(|v: f32| v.to_bits() as i32),
            InstructionKind::I64ReinterpretF64 => self.unop(|v: f64| v.to_bits() as i64),
            InstructionKind::F32ReinterpretI32 => self.unop(f32::from_bits),
            InstructionKind::F64ReinterpretI64 => self.unop(f64::from_bits),
            _ => unimplemented!(),
        };
        if self.stack.is_over_top_level() {
            return Ok(Signal::End);
        } else {
            return result;
        }
    }

    fn pop_as<T: NativeValue>(&mut self) -> ExecResult<T> {
        let value = self.stack.pop_value().map_err(Trap::Stack)?;
        T::from_value(value).ok_or(Trap::UnexpectedStackValueType(
            /* expected: */ T::value_type(),
            /* actual:   */ value.value_type(),
        ))
    }

    fn branch(&mut self, depth: u32, store: &Store) -> ExecResult<Signal> {
        let depth = depth as usize;
        let label = {
            let labels = self.stack.current_frame_labels().map_err(Trap::Stack)?;
            let labels_len = labels.len();
            assert!(depth + 1 <= labels_len);
            *labels[labels_len - depth - 1]
        };

        let arity = label.arity();

        let mut results = vec![];
        for _ in 0..arity {
            results.push(self.stack.pop_value().map_err(Trap::Stack)?);
        }

        for _ in 0..depth + 1 {
            self.stack.pop_while(|v| match v {
                StackValue::Value(_) => true,
                _ => false,
            });
            self.stack.pop_label().map_err(Trap::Stack)?;
        }

        for _ in 0..arity {
            self.stack.push_value(results.pop().unwrap());
        }

        // Jump to the continuation
        match label {
            Label::Loop(loop_label) => self.pc.loop_jump(&loop_label),
            Label::Return(_) => {
                return self.do_return(store);
            }
            Label::If(_) | Label::Block(_) => {
                let mut depth = depth + 1;
                loop {
                    let index = self.pc.inst_index().0 as usize;
                    match self.current_func_insts(store)?[index].kind {
                        InstructionKind::End => depth -= 1,
                        InstructionKind::Block { ty: _ } => depth += 1,
                        InstructionKind::If { ty: _ } => depth += 1,
                        InstructionKind::Loop { ty: _ } => depth += 1,
                        _ => (),
                    }
                    self.pc.inc_inst_index();
                    if depth == 0 {
                        break;
                    }
                }
            }
        }
        Ok(Signal::Next)
    }

    fn testop<T: NativeValue, F: Fn(T) -> bool>(&mut self, f: F) -> ExecResult<Signal> {
        self.unop(|a| Value::I32(if f(a) { 1 } else { 0 }))
    }

    fn relop<T: NativeValue, F: Fn(T, T) -> bool>(&mut self, f: F) -> ExecResult<Signal> {
        self.binop(|a: T, b: T| Value::I32(if f(a, b) { 1 } else { 0 }))
    }

    fn try_binop<T: NativeValue, To: Into<Value>, F: Fn(T, T) -> Result<To, value::Error>>(
        &mut self,
        f: F,
    ) -> ExecResult<Signal> {
        let rhs = self.pop_as()?;
        let lhs = self.pop_as()?;
        self.stack
            .push_value(f(lhs, rhs).map(|v| v.into()).map_err(Trap::Value)?);
        Ok(Signal::Next)
    }

    fn binop<T: NativeValue, To: Into<Value>, F: Fn(T, T) -> To>(
        &mut self,
        f: F,
    ) -> ExecResult<Signal> {
        let rhs = self.pop_as()?;
        let lhs = self.pop_as()?;
        self.stack.push_value(f(lhs, rhs).into());
        Ok(Signal::Next)
    }

    fn try_unop<From: NativeValue, To: Into<Value>, F: Fn(From) -> Result<To, value::Error>>(
        &mut self,
        f: F,
    ) -> ExecResult<Signal> {
        let v: From = self.pop_as()?;
        self.stack
            .push_value(f(v).map(|v| v.into()).map_err(Trap::Value)?);
        Ok(Signal::Next)
    }

    fn unop<From: NativeValue, To: Into<Value>, F: Fn(From) -> To>(
        &mut self,
        f: F,
    ) -> ExecResult<Signal> {
        let v: From = self.pop_as()?;
        self.stack.push_value(f(v).into());
        Ok(Signal::Next)
    }

    fn invoke<I: Interceptor>(
        &mut self,
        addr: FuncAddr,
        store: &Store,
        interceptor: &I,
    ) -> ExecResult<Signal> {
        let (func, exec_addr) = store.func(addr).ok_or(Trap::UndefinedFunc(addr.1))?;

        let mut args = Vec::new();
        let mut found_mismatch = false;
        for _ in func.ty().params.iter() {
            match self.stack.pop_value() {
                Ok(val) => args.push(val),
                Err(_) => found_mismatch = true,
            }
        }

        if found_mismatch {
            return Err(Trap::DirectCallTypeMismatch {
                func_name: func.name().to_string(),
                actual: args.iter().map(|v| v.value_type()).collect(),
                expected: func.ty().params.to_vec(),
            })
        }
        args.reverse();

        let arity = func.ty().returns.len();
        match func {
            FunctionInstance::Defined(func) => {
                let pc = ProgramCounter::new(func.module_index(), exec_addr, InstIndex::zero());
                let frame = CallFrame::new_from_func(exec_addr, &func, args, Some(self.pc));
                self.stack.set_frame(frame).map_err(Trap::Stack)?;
                self.stack.push_label(Label::Return(arity));
                self.pc = pc;
                interceptor.invoke_func(func.name())
            }
            FunctionInstance::Host(func) => {
                let mut result = Vec::new();
                func.code()
                    .call(&args, &mut result, store, addr.module_index())?;
                assert_eq!(result.len(), arity);
                for v in result {
                    self.stack.push_value(v);
                }
                Ok(Signal::Next)
            }
        }
    }
    fn do_return(&mut self, store: &Store) -> ExecResult<Signal> {
        let ret_pc = self.stack.current_frame().map_err(Trap::Stack)?.ret_pc;
        let func = store.func_global(self.pc.exec_addr());
        let arity = func.ty().returns.len();
        let mut result = vec![];
        for _ in 0..arity {
            result.push(self.stack.pop_value().map_err(Trap::Stack)?);
        }
        self.stack.pop_while(|v| match v {
            StackValue::Activation(_) => false,
            _ => true,
        });
        self.stack.pop_frame().map_err(Trap::Stack)?;
        for v in result {
            self.stack.push_value(v);
        }

        if let Some(ret_pc) = ret_pc {
            self.pc = ret_pc;
        }
        Ok(Signal::Next)
    }

    fn set_local(&mut self, index: usize) -> ExecResult<Signal> {
        let value = self.stack.pop_value().map_err(Trap::Stack)?;
        self.stack.set_local(index, value).map_err(Trap::Stack)?;

        Ok(Signal::Next)
    }

    fn memory(&self, store: &Store) -> ExecResult<std::rc::Rc<std::cell::RefCell<MemoryInstance>>> {
        let frame = self.stack.current_frame().map_err(Trap::Stack)?;
        let mem_addr = MemoryAddr::new_unsafe(frame.module_index(), 0);
        Ok(store.memory(mem_addr))
    }

    fn store<T: NativeValue + IntoLittleEndian>(
        &mut self,
        offset: usize,
        store: &Store,
    ) -> ExecResult<Signal> {
        let val: T = self.pop_as()?;
        let base_addr: i32 = self.pop_as()?;
        let base_addr = base_addr as usize;
        let addr = base_addr + offset;
        let mut buf: Vec<u8> = std::iter::repeat(0)
            .take(std::mem::size_of::<T>())
            .collect();
        val.into_le(&mut buf);
        self.memory(store)?
            .borrow_mut()
            .store(addr, &buf)
            .map_err(Trap::Memory)?;
        Ok(Signal::Next)
    }

    fn store_with_width<T: NativeValue + IntoLittleEndian>(
        &mut self,
        offset: usize,
        width: usize,
        store: &Store,
    ) -> ExecResult<Signal> {
        let val: T = self.pop_as()?;
        let base_addr: i32 = self.pop_as()?;
        let base_addr = base_addr as usize;
        let addr: usize = base_addr + offset;
        let mut buf: Vec<u8> = std::iter::repeat(0)
            .take(std::mem::size_of::<T>())
            .collect();
        val.into_le(&mut buf);
        let buf: Vec<u8> = buf.into_iter().take(width).collect();
        self.memory(store)?
            .borrow_mut()
            .store(addr, &buf)
            .map_err(Trap::Memory)?;
        Ok(Signal::Next)
    }

    fn load<T>(&mut self, offset: usize, store: &Store) -> ExecResult<Signal>
    where
        T: NativeValue + FromLittleEndian,
        T: Into<Value>,
    {
        let base_addr: i32 = self.pop_as()?;
        let base_addr = base_addr as usize;
        let addr: usize = base_addr + offset;

        let result: T = self
            .memory(store)?
            .borrow_mut()
            .load_as(addr)
            .map_err(Trap::Memory)?;
        self.stack.push_value(result.into());
        Ok(Signal::Next)
    }

    fn load_extend<T: FromLittleEndian + ExtendInto<U>, U: Into<Value>>(
        &mut self,
        offset: usize,
        store: &Store,
    ) -> ExecResult<Signal> {
        let base_addr: i32 = self.pop_as()?;
        let base_addr = base_addr as usize;
        let addr: usize = base_addr + offset;

        let result: T = self
            .memory(store)?
            .borrow_mut()
            .load_as(addr)
            .map_err(Trap::Memory)?;
        let result = result.extend_into();
        self.stack.push_value(result.into());
        Ok(Signal::Next)
    }
}

use anyhow;
use wasmparser::InitExpr;
pub fn eval_const_expr(
    init_expr: &InitExpr,
    store: &Store,
    module_index: ModuleIndex,
) -> anyhow::Result<Value> {
    use super::inst::transform_inst;
    let mut reader = init_expr.get_operators_reader();
    let base_offset = reader.original_position();
    let inst = transform_inst(&mut reader, base_offset)?;
    let val = match inst.kind {
        InstructionKind::I32Const { value } => Value::I32(value),
        InstructionKind::I64Const { value } => Value::I64(value),
        InstructionKind::F32Const { value } => Value::F32(value.bits()),
        InstructionKind::F64Const { value } => Value::F64(value.bits()),
        InstructionKind::GlobalGet { global_index } => {
            let addr = GlobalAddr::new_unsafe(module_index, global_index as usize);
            store.global(addr).borrow().value()
        }
        _ => panic!("Unsupported init_expr {:?}", inst.kind),
    };
    Ok(val)
}

#[derive(Debug)]
pub enum WasmError {
    ExecutionError(Trap),
    EntryFunctionNotFound(String),
    ReturnValueError(ReturnValError),
    HostExecutionError,
}

impl std::fmt::Display for WasmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmError::ExecutionError(err) => write!(f, "Failed to execute: {}", err),
            WasmError::EntryFunctionNotFound(func_name) => {
                write!(f, "Entry function \"{}\" not found", func_name)
            }
            WasmError::ReturnValueError(err) => {
                write!(f, "Failed to get returned value: {:?}", err)
            }
            WasmError::HostExecutionError => write!(f, "Failed to execute host func"),
        }
    }
}

pub fn simple_invoke_func(
    func_addr: FuncAddr,
    arguments: Vec<Value>,
    store: &mut Store,
) -> Result<Vec<Value>, WasmError> {
    match store
        .func(func_addr)
        .ok_or(WasmError::ExecutionError(Trap::UndefinedFunc(func_addr.1)))?
    {
        (FunctionInstance::Host(host), _) => {
            let mut results = Vec::new();
            match host
                .code()
                .call(&arguments, &mut results, store, func_addr.module_index())
            {
                Ok(_) => Ok(results),
                Err(_) => Err(WasmError::HostExecutionError),
            }
        }
        (FunctionInstance::Defined(func), exec_addr) => {
            let (frame, ret_types) = {
                let ret_types = &func.ty().returns;
                let frame = CallFrame::new_from_func(exec_addr, func, arguments, None);
                (frame, ret_types)
            };
            let pc = ProgramCounter::new(func.module_index(), exec_addr, InstIndex::zero());
            let interceptor = NopInterceptor::new();
            let mut executor = Executor::new(frame, ret_types.len(), pc);
            loop {
                let result = executor.execute_step(store, &interceptor);
                match result {
                    Ok(Signal::Next) => continue,
                    Ok(Signal::Breakpoint) => continue,
                    Ok(Signal::End) => match executor.pop_result(ret_types.to_vec()) {
                        Ok(values) => return Ok(values),
                        Err(err) => return Err(WasmError::ReturnValueError(err)),
                    },
                    Err(err) => return Err(WasmError::ExecutionError(err)),
                }
            }
        }
    }
}
