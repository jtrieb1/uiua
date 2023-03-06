use std::{cmp::Ordering, fmt, mem::swap};

use crate::{
    array::Array,
    ast::FunctionId,
    builtin::{Env, Op1, Op2},
    compile::Assembly,
    lex::Span,
    list::List,
    value::{Function, Partial, Value},
    RuntimeResult, TraceFrame, UiuaError, UiuaResult,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Instr {
    Comment(String),
    Push(Value),
    Constant(usize),
    List(usize),
    Array(usize),
    /// Copy the nth value from the top of the stack
    CopyRel(usize),
    /// Copy the nth value from the bottom of the stack
    CopyAbs(usize),
    Swap,
    Move(usize),
    Rotate(usize),
    Pop(usize),
    Call(usize),
    Return,
    Jump(isize),
    PopJumpIf(isize, bool),
    JumpIfElsePop(isize, bool),
    Op1(Op1),
    Op2(Op2, usize),
    DestructureList(usize, Box<Span>),
    PushUnresolvedFunction(Box<FunctionId>),
    Dud,
}

fn _keep_instr_small(_: std::convert::Infallible) {
    let _: [u8; 32] = unsafe { std::mem::transmute(Instr::Dud) };
}

impl fmt::Display for Instr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Instr::Comment(s) => write!(f, "\n// {}", s),
            instr => write!(f, "{instr:?}"),
        }
    }
}

struct StackFrame {
    stack_size: usize,
    function: Function,
    ret: usize,
    call_span: usize,
}

pub const DBG: bool = false;
macro_rules! dprintln {
    ($($arg:tt)*) => {
        if $crate::vm::DBG {
            println!($($arg)*);
        }
    };
}
pub(crate) use dprintln;

#[derive(Default)]
pub struct Vm {
    call_stack: Vec<StackFrame>,
    pub stack: Vec<Value>,
    pc: usize,
}

impl Vm {
    pub fn run_assembly(&mut self, assembly: &Assembly) -> UiuaResult<Option<Value>> {
        match self.run_assembly_inner(assembly) {
            Ok(val) => Ok(val),
            Err(error) => {
                let mut trace = Vec::new();
                for frame in self.call_stack.iter().rev() {
                    let info = assembly.function_info(frame.function);
                    trace.push(TraceFrame {
                        id: info.id.clone(),
                        span: assembly.spans[frame.call_span].clone(),
                    });
                }
                Err(UiuaError::Run {
                    error: error.into(),
                    trace,
                })
            }
        }
    }
    fn run_assembly_inner(&mut self, assembly: &Assembly) -> RuntimeResult<Option<Value>> {
        let stack = &mut self.stack;
        let call_stack = &mut self.call_stack;
        dprintln!("\nRunning...");
        if self.pc == 0 {
            self.pc = assembly.start;
        }
        #[cfg(feature = "profile")]
        let mut i = 0;
        let pc = &mut self.pc;
        while *pc < assembly.instrs.len() {
            #[cfg(feature = "profile")]
            if i % 100_000 == 0 {
                puffin::GlobalProfiler::lock().new_frame();
            }
            #[cfg(feature = "profile")]
            {
                i += 1;
            }
            #[cfg(feature = "profile")]
            puffin::profile_scope!("instr loop");
            let instr = &assembly.instrs[*pc];
            dprintln!("{pc:>3} {instr}");
            match instr {
                Instr::Comment(_) => {}
                Instr::Push(v) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("push");
                    stack.push(v.clone())
                }
                Instr::Constant(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("constant");
                    stack.push(assembly.constants[*n].clone())
                }
                Instr::List(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("list");
                    let list = List::from_items(stack.drain(stack.len() - *n..));
                    stack.push(list.into());
                }
                Instr::Array(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("list");
                    let array: Array = stack.drain(stack.len() - *n..).collect();
                    stack.push(array.into());
                }
                Instr::CopyRel(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("copy");
                    stack.push(stack[stack.len() - *n].clone())
                }
                Instr::CopyAbs(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("copy");
                    stack.push(stack[*n].clone())
                }
                Instr::Swap => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("swap");
                    let len = stack.len();
                    stack.swap(len - 1, len - 2);
                }
                Instr::Move(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("remove");
                    let val = stack.remove(stack.len() - *n);
                    stack.push(val);
                }
                Instr::Rotate(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("rotate");
                    let val = stack.pop().unwrap();
                    stack.insert(stack.len() + 1 - *n, val);
                }
                Instr::Pop(n) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("pop");
                    stack.truncate(stack.len() - *n);
                }
                Instr::Call(span) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("call");
                    let value = stack.pop().unwrap();
                    let (function, partial_count) = match value {
                        Value::Function(f) => (f, 0),
                        Value::Partial(p) => {
                            let arg_count = p.args.len();
                            for arg in p.args.iter() {
                                stack.push(arg.clone());
                            }
                            (p.function, arg_count)
                        }
                        Value::Byte(_) | Value::Int(_) | Value::List(_) | Value::Array(_) => {
                            stack.push(value);
                            (assembly.cached_functions.get, 1)
                        }
                        _ => {
                            let message = format!("Cannot call {}", value.ty());
                            return Err(assembly.spans[*span].error(message));
                        }
                    };
                    // Handle partial arguments
                    let arg_count = partial_count + 1;
                    if partial_count > 0 {
                        dprintln!("{stack:?}");
                    }
                    // Call
                    let info = assembly.function_info(function);
                    match arg_count.cmp(&info.params) {
                        Ordering::Less => {
                            let partial = Partial {
                                function,
                                args: stack.drain(stack.len() - arg_count..).collect(),
                            };
                            stack.push(partial.into());
                        }
                        Ordering::Equal => {
                            call_stack.push(StackFrame {
                                stack_size: stack.len() - arg_count,
                                function,
                                ret: *pc + 1,
                                call_span: *span,
                            });
                            *pc = function.0;
                            continue;
                        }
                        Ordering::Greater => {
                            let message = format!(
                                "Too many arguments to {}: expected {}, got {}",
                                info.id, info.params, arg_count
                            );
                            return Err(assembly.spans[*span].clone().sp(message));
                        }
                    }
                }
                Instr::Return => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("return");
                    if let Some(frame) = call_stack.pop() {
                        let value = stack.pop().unwrap();
                        *pc = frame.ret;
                        stack.truncate(frame.stack_size);
                        stack.push(value);
                        dprintln!("{:?}", stack);
                        continue;
                    } else {
                        break;
                    }
                }
                Instr::Jump(delta) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("jump");
                    *pc = pc.wrapping_add_signed(*delta);
                    continue;
                }
                Instr::PopJumpIf(delta, cond) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("jump_if");
                    let val = stack.pop().unwrap();
                    if val.is_truthy() == *cond {
                        *pc = pc.wrapping_add_signed(*delta);
                        continue;
                    }
                }
                Instr::JumpIfElsePop(delta, cond) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("jump_if_else_pop");
                    let val = stack.last().unwrap();
                    if val.is_truthy() == *cond {
                        *pc = pc.wrapping_add_signed(*delta);
                        continue;
                    } else {
                        stack.pop();
                    }
                }
                Instr::Op1(op) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("builtin_op1");
                    let val = stack.last_mut().unwrap();
                    let env = Env { assembly, span: 0 };
                    val.op1(*op, env)?;
                }
                Instr::Op2(op, span) => {
                    #[cfg(feature = "profile")]
                    puffin::profile_scope!("builtin_op2");
                    let (left, right) = {
                        #[cfg(feature = "profile")]
                        puffin::profile_scope!("builtin_op2_args");
                        let mut iter = stack.iter_mut().rev();
                        let right = iter.next().unwrap();
                        let left = iter.next().unwrap();
                        (left, right)
                    };
                    swap(left, right);
                    let env = Env {
                        assembly,
                        span: *span,
                    };
                    left.op2(right, *op, env)?;
                    stack.pop();
                }
                Instr::DestructureList(_n, _span) => {
                    // let list = match stack.pop().unwrap() {
                    //     Value::List(list) if *n == list.len() => list,
                    //     Value::List(list) => {
                    //         let message =
                    //             format!("cannot destructure list of {} as list of {n}", list.len());
                    //         return Err(span.sp(message).into());
                    //     }
                    //     val => {
                    //         let message =
                    //             format!("cannot destructure {} as list of {}", val.ty(), n);
                    //         return Err(span.sp(message).into());
                    //     }
                    // };
                    // for val in list.into_iter().rev() {
                    //     stack.push(val);
                    // }
                }
                Instr::PushUnresolvedFunction(id) => {
                    panic!("unresolved function: {id:?}")
                }
                Instr::Dud => {
                    panic!("unresolved instruction")
                }
            }
            dprintln!("{:?}", stack);
            *pc += 1;
        }
        *pc -= 1;
        Ok(stack.pop())
    }
}