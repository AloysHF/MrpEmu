//! MiniJ bytecode interpreter: the Lua 5.0.2 semantics subset the platform
//! scripts need.
//!
//! The value model, register stack, closures with open upvalues, tables,
//! and the 35-opcode execution loop follow `lua-5.0.2/src/lvm.c`. Native
//! functions (the platform API surface) are registered in a table and
//! receive their arguments as a slice, returning any number of results —
//! the bridge to the `mrc_`/ext machinery lives in those natives.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::opcodes::{Instruction, Op};
use crate::undump::Proto;

/// Register/constant split used by the `RK` fields (`llimits.h`).
const MAXSTACK: usize = 250;

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Value {
    Nil,
    Bool(bool),
    Num(f32),
    Str(Rc<Vec<u8>>),
    Table(Rc<RefCell<Table>>),
    Closure(Rc<LuaClosure>),
    /// Index into the VM's native-function registry.
    Native(u16),
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Nil, Value::Nil) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Num(x), Value::Num(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Table(x), Value::Table(y)) => Rc::ptr_eq(x, y),
            (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
            (Value::Native(x), Value::Native(y)) => x == y,
            _ => false,
        }
    }
}

impl Value {
    pub fn str_from(bytes: &[u8]) -> Value {
        Value::Str(Rc::new(bytes.to_vec()))
    }

    pub fn is_false_y(&self) -> bool {
        matches!(self, Value::Nil | Value::Bool(false))
    }

    pub fn as_num(&self) -> Option<f32> {
        match self {
            Value::Num(n) => Some(*n),
            // Lua coerces numeric strings in arithmetic.
            Value::Str(s) => std::str::from_utf8(s)
                .ok()
                .and_then(|text| text.trim().parse::<f32>().ok()),
            _ => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Table(_) => "table",
            Value::Closure(_) | Value::Native(_) => "function",
        }
    }
}

/// Hashable projection of a table key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    Bool(bool),
    /// f32 bits; -0.0 normalizes to 0.0 and NaN keys are rejected.
    Num(u32),
    Str(Vec<u8>),
    Table(usize),
    Closure(usize),
    Native(u16),
}

impl Key {
    pub(crate) fn sort_bits(&self) -> (u8, u32) {
        match self {
            Key::Bool(flag) => (0, *flag as u32),
            Key::Num(bits) => (1, *bits),
            Key::Str(bytes) => (2, bytes.len() as u32),
            Key::Table(id) => (3, *id as u32),
            Key::Closure(id) => (4, *id as u32),
            Key::Native(id) => (5, *id as u32),
        }
    }
}

fn key_of(value: &Value) -> Option<Key> {
    match value {
        Value::Nil => None,
        Value::Bool(b) => Some(Key::Bool(*b)),
        Value::Num(n) => {
            if n.is_nan() {
                None
            } else {
                let bits = if *n == 0.0 { 0.0f32 } else { *n }.to_bits();
                Some(Key::Num(bits))
            }
        }
        Value::Str(s) => Some(Key::Str(s.as_ref().to_vec())),
        Value::Table(t) => Some(Key::Table(Rc::as_ptr(t) as *const u8 as usize)),
        Value::Closure(c) => Some(Key::Closure(Rc::as_ptr(c) as *const u8 as usize)),
        Value::Native(i) => Some(Key::Native(*i)),
    }
}

pub type UpVal = Rc<RefCell<Value>>;

#[derive(Debug)]
pub struct LuaClosure {
    pub proto: Rc<Proto>,
    pub upvals: Vec<UpVal>,
}

#[derive(Debug, Default)]
pub struct Table {
    pub array: Vec<Value>,
    pub(crate) map: HashMap<Key, Value>,
    pub metatable: Option<Rc<RefCell<Table>>>,
}

impl Table {
    pub fn get(&self, key: &Value) -> Value {
        if let Value::Num(n) = key {
            let index = *n;
            if index.fract() == 0.0 && index >= 1.0 {
                let index = index as usize;
                if let Some(value) = self.array.get(index - 1) {
                    return value.clone();
                }
            }
        }
        match key_of(key) {
            Some(key) => self.map.get(&key).cloned().unwrap_or(Value::Nil),
            None => Value::Nil,
        }
    }

    pub fn set(&mut self, key: Value, value: Value) {
        if let Value::Num(n) = &key {
            let index = *n;
            if index.fract() == 0.0 && index >= 1.0 {
                let index = index as usize;
                if index <= self.array.len() + 1 && !matches!(value, Value::Nil) {
                    if index == self.array.len() + 1 {
                        self.array.push(value);
                        return;
                    }
                    self.array[index - 1] = value;
                    return;
                }
            }
        }
        if let Some(key) = key_of(&key) {
            if matches!(value, Value::Nil) {
                self.map.remove(&key);
            } else {
                self.map.insert(key, value);
            }
        } // nil/NaN keys are silently dropped, like Lua
    }

    pub fn len(&self) -> usize {
        self.array.len()
    }

    pub fn is_empty(&self) -> bool {
        self.array.is_empty() && self.map.is_empty()
    }

    /// Snapshot of the hash part, sorted for deterministic iteration.
    pub fn snapshot_map(&self) -> Vec<(Key, Value)> {
        let mut entries: Vec<(Key, Value)> = self
            .map
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        entries.sort_by_key(|(key, _)| key.sort_bits());
        entries
    }
}

// ---------------------------------------------------------------------------
// Interpreter state
// ---------------------------------------------------------------------------

pub type Native = Rc<dyn Fn(&mut Vm, &[Value]) -> Result<Vec<Value>, String>>;

/// Diagnostic switch: when `MRP_TRACE_CALLS` is set at boot, every Call
/// whose callee register holds nil is logged with its chunk and pc.
static CALL_NIL_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn call_nil_trace_enabled() -> bool {
    *CALL_NIL_TRACE.get_or_init(|| std::env::var("MRP_TRACE_CALLS").is_ok())
}

/// Errors surfaced by the interpreter (script `error()` and internal faults).
#[derive(Debug, Clone)]
pub struct VmError(pub String);

pub struct Vm {
    pub globals: Rc<RefCell<Table>>,
    natives: Vec<Native>,
    native_names: Vec<&'static str>,
    stack: Vec<Value>,
    open_upvals: Vec<(usize, UpVal)>,
    frames: usize,
    /// Stack top marker for variable-result sequences (B==0/C==0 calls).
    pub top: usize,
    /// Host state the platform natives operate on: the running ext guest,
    /// the presented screen, and the loaded package.
    pub guest: Option<crate::cfunction::Guest>,
    pub screen: crate::display::Screen,
    pub package: Option<crate::package::MrpPackage>,
    /// Native slot backing unknown globals (registered by the bridge).
    pub stub_native: Option<u16>,
    /// Names of globals the script read but the host never registered.
    pub missing_globals: RefCell<Vec<String>>,
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    pub fn new() -> Self {
        Self {
            globals: Rc::new(RefCell::new(Table::default())),
            natives: Vec::new(),
            native_names: Vec::new(),
            stack: Vec::new(),
            open_upvals: Vec::new(),
            frames: 0,
            top: 0,
            guest: None,
            screen: crate::display::Screen::new(
                crate::display::DEFAULT_WIDTH,
                crate::display::DEFAULT_HEIGHT,
            ),
            package: None,
            stub_native: None,
            missing_globals: RefCell::new(Vec::new()),
        }
    }

    /// Registers a native function under a global name.
    pub fn add_native(&mut self, name: &'static str, native: Native) -> u16 {
        let index = self.natives.len() as u16;
        self.natives.push(native);
        self.native_names.push(name);
        index
    }

    pub fn register(&mut self, name: &'static str, native: Native) {
        let index = self.add_native(name, native);
        self.globals
            .borrow_mut()
            .set(Value::str_from(name.as_bytes()), Value::Native(index));
    }

    /// Calls a global function value with the given arguments.
    pub fn call_global(&mut self, name: &str, args: &[Value]) -> Result<Vec<Value>, String> {
        let func = self.globals.borrow().get(&Value::str_from(name.as_bytes()));
        match func {
            Value::Closure(closure) => self.call_closure(closure, args),
            Value::Native(index) => self.call_native(index, args),
            other => Err(format!(
                "global `{name}` is not callable (a {})",
                other.type_name()
            )),
        }
    }

    /// Registers a native inside a table stored in the globals.
    pub fn register_in(&mut self, table_name: &'static str, name: &'static str, native: Native) {
        let index = self.natives.len() as u16;
        self.natives.push(native);
        self.native_names.push(name);
        let table = {
            let mut globals = self.globals.borrow_mut();
            let entry = globals.get(&Value::str_from(table_name.as_bytes()));
            match entry {
                Value::Table(table) => table,
                _ => {
                    let table = Rc::new(RefCell::new(Table::default()));
                    globals.set(
                        Value::str_from(table_name.as_bytes()),
                        Value::Table(table.clone()),
                    );
                    table
                }
            }
        };
        table
            .borrow_mut()
            .set(Value::str_from(name.as_bytes()), Value::Native(index));
    }

    /// Exposes an already-registered native under an additional global name
    /// (the platform's `_lowerCamel` aliases, e.g. `_num` for `tonumber`).
    pub fn alias(&mut self, existing: &'static str, alias: &'static str) {
        if let Some((index, _)) = self
            .native_names
            .iter()
            .enumerate()
            .find(|(_, name)| **name == existing)
        {
            self.globals.borrow_mut().set(
                Value::str_from(alias.as_bytes()),
                Value::Native(index as u16),
            );
        }
    }

    /// Looks up a native's index by registration name.
    pub fn native_index(&self, name: &str) -> Option<u16> {
        self.native_names
            .iter()
            .position(|entry| *entry == name)
            .map(|index| index as u16)
    }

    /// Runs a parsed chunk as the main function with no arguments.
    pub fn run_chunk(&mut self, proto: Rc<Proto>) -> Result<(), VmError> {
        let closure = Rc::new(LuaClosure {
            proto,
            upvals: Vec::new(),
        });
        let results = self.call_closure(closure, &[]).map_err(VmError)?;
        let _ = results;
        Ok(())
    }

    fn close_upvals(&mut self, from: usize) {
        self.open_upvals.retain(|(index, upval)| {
            if *index >= from {
                *upval.borrow_mut() = self.stack[*index].clone();
                false
            } else {
                true
            }
        });
    }

    fn find_upval(&mut self, index: usize) -> UpVal {
        for (existing, upval) in &self.open_upvals {
            if *existing == index {
                return upval.clone();
            }
        }
        let upval: UpVal = Rc::new(RefCell::new(self.stack[index].clone()));
        self.open_upvals.push((index, upval.clone()));
        upval
    }

    /// Grows the stack so that `[0..index)` is addressable with nils.
    fn pad_stack(&mut self, index: usize) {
        if index > self.stack.len() {
            self.stack.resize(index, Value::Nil);
        }
    }

    fn set_stack(&mut self, index: usize, value: Value) {
        self.pad_stack(index + 1);
        self.stack[index] = value;
    }

    /// Calls a closure with the given arguments. Public so that native
    /// functions (e.g. `pcall`) can re-enter the interpreter.
    pub fn call_closure(
        &mut self,
        closure: Rc<LuaClosure>,
        args: &[Value],
    ) -> Result<Vec<Value>, String> {
        self.frames += 1;
        if self.frames > 200 {
            self.frames -= 1;
            return Err("stack overflow (recursion limit)".to_string());
        }
        if self.frames > 50 && std::env::var("MRP_TRACE").is_ok() {
            eprintln!(
                "[depth {}] pc_base={:p} src={:?}",
                self.frames,
                Rc::as_ptr(&closure.proto),
                closure.proto.source
            );
        }
        let base = self.stack.len();
        let frame_size = (closure.proto.max_stack_size as usize)
            .max(closure.proto.num_params as usize)
            .max(args.len())
            .max(1024);
        self.stack.extend(
            args.iter()
                .cloned()
                .chain(std::iter::repeat(Value::Nil))
                .take(frame_size),
        );
        let result = self.execute(closure.clone(), base);
        self.close_upvals(base);
        self.frames -= 1;
        let results = match result {
            Ok(results) => results,
            Err(error) => {
                self.stack.truncate(base);
                // Prepend this frame's chunk name so the error carries the
                // full chunk chain from the innermost frame outward.
                let source = closure
                    .proto
                    .source
                    .as_deref()
                    .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                    .unwrap_or_else(|| "<anon>".to_string());
                return Err(format!("{source} -> {error}"));
            }
        };
        self.stack.truncate(base);
        Ok(results)
    }

    /// Executes one closure frame; returns its returned values.
    fn execute(&mut self, closure: Rc<LuaClosure>, base: usize) -> Result<Vec<Value>, String> {
        let proto = closure.proto.clone();
        let code_len = proto.code.len();
        let mut pc = 0usize;
        loop {
            if pc >= code_len {
                return Ok(vec![Value::Nil]);
            }
            let inst = proto.code[pc];
            pc += 1;
            let op = Op::from_u8(inst.op())
                .ok_or_else(|| format!("invalid opcode {} at {pc}", inst.op()))?;
            let (a, b, c, bx, sbx) = (
                inst.a() as usize,
                inst.b() as usize,
                inst.c() as usize,
                inst.bx(),
                inst.sbx(),
            );
            if std::env::var("MRP_TRACE").is_ok() {
                eprintln!(
                    "[vm {pc}] word={:010x} {op:?} a={a} b={b} c={c} bx={bx}",
                    inst.0
                );
            }
            match op {
                Op::Move => self.set_stack(base + a, self.stack[base + b].clone()),
                Op::LoadK => {
                    let value = constant_value(&proto, bx as usize)?;
                    self.set_stack(base + a, value);
                }
                Op::LoadBool => {
                    self.set_stack(base + a, Value::Bool(b != 0));
                    if c != 0 {
                        pc += 1;
                    }
                }
                Op::LoadNil => {
                    for index in 0..=b {
                        self.set_stack(base + a + index, Value::Nil);
                    }
                }
                Op::GetUpval => {
                    let value = closure.upvals[b].borrow().clone();
                    self.set_stack(base + a, value);
                }
                Op::Setupval => {
                    *closure.upvals[b].borrow_mut() = self.stack[base + a].clone();
                }
                Op::GetGlobal => {
                    let name = constant_value(&proto, bx as usize)?;
                    let value = self.globals.borrow().get(&name);
                    if matches!(value, Value::Nil) {
                        if let Value::Str(text) = &name {
                            let mut missing = self.missing_globals.borrow_mut();
                            if !missing
                                .iter()
                                .any(|entry| entry.as_bytes() == text.as_slice())
                            {
                                let source = closure
                                    .proto
                                    .source
                                    .as_deref()
                                    .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                                    .unwrap_or_default();
                                log::warn!(
                                    "global `{}` missed in {source} at pc {}",
                                    String::from_utf8_lossy(text),
                                    pc
                                );
                                missing.push(String::from_utf8_lossy(text).to_string());
                            }
                        }
                    }
                    // Lua semantics: a missing global reads as nil. The old
                    // stub substitution broke  checks in scripts.
                    self.set_stack(base + a, value);
                }
                Op::SetGlobal => {
                    let name = constant_value(&proto, bx as usize)?;
                    let value = self.stack[base + a].clone();
                    self.globals.borrow_mut().set(name, value);
                }
                Op::GetTable => {
                    let table = self.stack[base + b].clone();
                    let key = self.rk(&proto, base, c)?;
                    let result = self
                        .table_get(&table, &key)
                        .map_err(|error| format!("{error} (gettable at pc {pc})"))?;
                    self.set_stack(base + a, result);
                }
                Op::SetTable => {
                    let table = self.stack[base + a].clone();
                    let key = self.rk(&proto, base, b)?;
                    let value = self.rk(&proto, base, c)?;
                    self.table_set(&table, &key, value)?;
                }
                Op::NewTable => {
                    self.set_stack(
                        base + a,
                        Value::Table(Rc::new(RefCell::new(Table::default()))),
                    );
                }
                Op::Self_ => {
                    let object = self.stack[base + b].clone();
                    let key = self.rk(&proto, base, c)?;
                    let method = self.table_get(&object, &key)?;
                    self.set_stack(base + a + 1, object);
                    self.set_stack(base + a, method);
                }
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Pow => {
                    let lhs = self.rk(&proto, base, b)?;
                    let rhs = self.rk(&proto, base, c)?;
                    let result = arith(op, &lhs, &rhs)?;
                    self.set_stack(base + a, result);
                }
                Op::Unm => {
                    let value = self.stack[base + b].clone();
                    let number = value
                        .as_num()
                        .ok_or_else(|| "attempt to negate a non-number".to_string())?;
                    self.set_stack(base + a, Value::Num(-number));
                }
                Op::Not => {
                    let value = self.stack[base + b].clone();
                    self.set_stack(base + a, Value::Bool(value.is_false_y()));
                }
                Op::Concat => {
                    let mut out = Vec::new();
                    for index in b..=c {
                        let value = self.stack[base + index].clone();
                        match &value {
                            Value::Str(text) => out.extend_from_slice(text),
                            Value::Num(number) => {
                                out.extend_from_slice(format_number(*number).as_bytes())
                            }
                            _ => {
                                return Err("attempt to concatenate a non-string value".to_string())
                            }
                        }
                    }
                    self.set_stack(base + a, Value::str_from(&out));
                }
                Op::Jmp => {
                    pc = (pc as i64 + i64::from(sbx)) as usize;
                }
                Op::Eq | Op::Lt | Op::Le => {
                    let lhs = self.rk(&proto, base, b)?;
                    let rhs = self.rk(&proto, base, c)?;
                    let outcome = compare(op, &lhs, &rhs)?;
                    if outcome != (a != 0) {
                        pc += 1;
                    }
                }
                Op::Test => {
                    // 5.0: the next instruction word carries the jump.
                    let rb = self.stack[base + b].clone();
                    if rb.is_false_y() as i32 == c as i32 {
                        pc += 1;
                    } else {
                        self.set_stack(base + a, rb);
                        let jump = proto.code[pc];
                        pc = (pc as i64 + 1 + i64::from(Instruction(jump.0).sbx())) as usize;
                    }
                }
                Op::Call => {
                    // Lua 5.0/5.1 calling convention: the callee sits in
                    // R[A], arguments follow at R[A+1] (B = arg count + 1,
                    // C = result count + 1; zero means "to top"/"all").
                    let func_reg = base + a;
                    if call_nil_trace_enabled()
                        && matches!(self.stack.get(func_reg), Some(Value::Nil))
                    {
                        let source = closure
                            .proto
                            .source
                            .as_deref()
                            .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                            .unwrap_or_default();
                        eprintln!(
                            "[call-nil {source} pc={pc}] a={a} b={b} c={c} callee_reg=R[{func_reg}]"
                        );
                    }
                    let nargs = if b == 0 {
                        self.top.saturating_sub(base + a + 1)
                    } else {
                        b - 1
                    };
                    self.pad_stack(base + a + 1 + nargs.max(1));
                    let args: Vec<Value> = self.stack[base + a + 1..base + a + 1 + nargs].to_vec();
                    let func = self.stack[func_reg].clone();
                    let nresults = if c == 0 {
                        None
                    } else {
                        Some(c.saturating_sub(1))
                    };
                    self.call_value_in_place(&func, &args, nresults, func_reg)
                        .map_err(|error| format!("{error} (call at pc {pc})"))?;
                    pc += 0;
                }
                Op::Tailcall => {
                    let func_reg = base + a;
                    let nargs = if b == 0 {
                        self.top.saturating_sub(base + a + 1)
                    } else {
                        b - 1
                    };
                    let args: Vec<Value> = self.stack[base + a + 1..base + a + 1 + nargs].to_vec();
                    let func = self.stack[func_reg].clone();
                    match func {
                        Value::Closure(inner) => {
                            // Replace the current frame in place: arguments
                            // move down onto the callee's register window.
                            self.close_upvals(base);
                            for (index, arg) in args.iter().enumerate() {
                                self.set_stack(base + index, arg.clone());
                            }
                            self.stack.truncate(
                                base + args.len().max(inner.proto.max_stack_size as usize),
                            );
                            return self.tail_execute(inner, base);
                        }
                        Value::Native(index) => {
                            let results = self.call_native(index, &args)?;
                            self.close_upvals(base);
                            self.stack.truncate(base);
                            return Ok(results);
                        }
                        other => {
                            return Err(format!(
                                "attempt to tail-call a {} value",
                                other.type_name()
                            ))
                        }
                    }
                }
                Op::Return => {
                    self.close_upvals(base);
                    let count = if b == 0 {
                        self.top.saturating_sub(base + a)
                    } else {
                        b - 1
                    };
                    let results: Vec<Value> = self.stack[base + a..base + a + count].to_vec();
                    self.stack.truncate(base);
                    return Ok(results);
                }
                Op::ForLoop => {
                    let step = self.stack[base + a + 2]
                        .as_num()
                        .ok_or_else(|| "`for' step must be a number".to_string())?;
                    let limit = self.stack[base + a + 1]
                        .as_num()
                        .ok_or_else(|| "`for' limit must be a number".to_string())?;
                    let index = self.stack[base + a]
                        .as_num()
                        .ok_or_else(|| "`for' initial value must be a number".to_string())?
                        + step;
                    self.set_stack(base + a, Value::Num(index));
                    let cont = if step > 0.0 {
                        index <= limit
                    } else {
                        index >= limit
                    };
                    if cont {
                        self.set_stack(base + a + 3, Value::Num(index));
                        pc = (pc as i64 + i64::from(sbx)) as usize;
                    }
                }
                Op::TForLoop => {
                    let nvar = c + 1;
                    let func = self.stack[base + a].clone();
                    let state = self.stack[base + a + 1].clone();
                    let control = self.stack[base + a + 2].clone();
                    let mut args = vec![state, control];
                    args.truncate(2);
                    let results = match &func {
                        Value::Closure(closure) => self.call_closure(closure.clone(), &args)?,
                        Value::Native(index) => self.call_native(*index, &args)?,
                        other => {
                            return Err(format!("attempt to iterate a {} value", other.type_name()))
                        }
                    };
                    for index in 0..nvar {
                        self.set_stack(
                            base + a + index,
                            results.get(index).cloned().unwrap_or(Value::Nil),
                        );
                    }
                    if self.stack[base + a].is_false_y() {
                        pc += 1;
                    } else {
                        let jump = proto.code[pc];
                        pc = (pc as i64 + 1 + i64::from(Instruction(jump.0).sbx())) as usize;
                    }
                }
                Op::TForPrep => {
                    // `for compatibility only` in 5.0: swap in next() for tables.
                    let object = self.stack[base + a].clone();
                    if matches!(object, Value::Table(_)) {
                        let next = self.globals.borrow().get(&Value::str_from(b"next"));
                        self.set_stack(base + a + 1, object);
                        self.set_stack(base + a, next);
                    }
                    pc = (pc as i64 + i64::from(sbx)) as usize;
                }
                Op::SetList => {
                    let table = match self.stack[base + a].clone() {
                        Value::Table(table) => table,
                        _ => return Err("SETLIST on a non-table".to_string()),
                    };
                    let count = ((bx & 31) + 1) as usize;
                    let start = (bx - (bx & 31)) as usize;
                    let mut table = table.borrow_mut();
                    if table.array.len() < start + count {
                        table.array.resize(start + count, Value::Nil);
                    }
                    for index in 0..count {
                        table.array[start + index] = self.stack[base + a + 1 + index].clone();
                    }
                }
                Op::SetListO => {
                    let table = match self.stack[base + a].clone() {
                        Value::Table(table) => table,
                        _ => return Err("SETLISTO on a non-table".to_string()),
                    };
                    let count = ((bx & 31) + 1) as usize;
                    let start = (bx - (bx & 31)) as usize;
                    let mut table = table.borrow_mut();
                    if table.array.len() < start + count {
                        table.array.resize(start + count, Value::Nil);
                    }
                    for index in 0..count {
                        if matches!(self.stack[base + a + 1 + index], Value::Nil) {
                            continue;
                        }
                        table.array[start + index] = self.stack[base + a + 1 + index].clone();
                    }
                }
                Op::Close => self.close_upvals(base + a),
                Op::Closure => {
                    let nested = proto
                        .protos
                        .get(bx as usize)
                        .ok_or_else(|| format!("CLOSURE references missing nested proto {bx}"))?;
                    let nested = Rc::new(nested.clone());
                    let mut upvals = Vec::with_capacity(nested.num_upvalues as usize);
                    for _ in 0..nested.num_upvalues {
                        let descriptor = proto.code[pc];
                        pc += 1;
                        let op = Op::from_u8(descriptor.op())
                            .ok_or_else(|| "invalid upvalue descriptor".to_string())?;
                        match op {
                            Op::GetUpval => {
                                let parent = closure
                                    .upvals
                                    .get(descriptor.b() as usize)
                                    .ok_or_else(|| "upvalue descriptor out of range".to_string())?;
                                upvals.push(parent.clone());
                            }
                            Op::Move => {
                                upvals.push(self.find_upval(base + descriptor.b() as usize));
                            }
                            _ => return Err("invalid upvalue descriptor opcode".to_string()),
                        }
                    }
                    self.set_stack(
                        base + a,
                        Value::Closure(Rc::new(LuaClosure {
                            proto: nested,
                            upvals,
                        })),
                    );
                }
            }
        }
    }

    /// Tail-call variant of [`Vm::execute`] that reuses the current frame.
    fn tail_execute(&mut self, closure: Rc<LuaClosure>, base: usize) -> Result<Vec<Value>, String> {
        self.execute(closure, base)
    }

    fn call_value_in_place(
        &mut self,
        func: &Value,
        args: &[Value],
        nresults: Option<usize>,
        place: usize,
    ) -> Result<(), String> {
        match func {
            Value::Closure(closure) => {
                let results = self.call_closure(closure.clone(), args)?;
                for index in 0..nresults.unwrap_or(results.len()) {
                    let value = results.get(index).cloned().unwrap_or(Value::Nil);
                    self.set_stack(place + index, value);
                }
                self.top = place + nresults.unwrap_or(results.len());
                Ok(())
            }
            Value::Native(index) => {
                let results = self.call_native(*index, args)?;
                for index in 0..nresults.unwrap_or(results.len()) {
                    let value = results.get(index).cloned().unwrap_or(Value::Nil);
                    self.set_stack(place + index, value);
                }
                self.top = place + nresults.unwrap_or(results.len());
                Ok(())
            }
            other => Err(format!("attempt to call a {} value", other.type_name())),
        }
    }

    /// Calls a registered native by index. Public so `pcall`-style natives
    /// can invoke function values.
    pub fn call_native(&mut self, index: u16, args: &[Value]) -> Result<Vec<Value>, String> {
        let native = self
            .natives
            .get(index as usize)
            .cloned()
            .ok_or_else(|| "native index out of range".to_string())?;
        // The native is shared through an Rc and never borrows the VM, so
        // re-entrant calls (nested dofile/pcall) are safe.
        native(self, args)
    }

    /// RK field: registers below MAXSTACK, constants above.
    fn rk(&self, proto: &Proto, base: usize, arg: usize) -> Result<Value, String> {
        if arg < MAXSTACK {
            Ok(self.stack.get(base + arg).cloned().unwrap_or(Value::Nil))
        } else {
            constant_value(proto, arg - MAXSTACK)
        }
    }

    fn table_get(&self, table: &Value, key: &Value) -> Result<Value, String> {
        match table {
            Value::Table(table) => Ok(table.borrow().get(key)),
            _ => Err(format!("attempt to index a {} value", table.type_name())),
        }
    }

    fn table_set(&mut self, table: &Value, key: &Value, value: Value) -> Result<(), String> {
        match table {
            Value::Table(table) => {
                table.borrow_mut().set(key.clone(), value);
                Ok(())
            }
            _ => Err(format!("attempt to index a {} value", table.type_name())),
        }
    }
}

fn constant_value(proto: &Proto, index: usize) -> Result<Value, String> {
    match proto.constants.get(index) {
        Some(crate::undump::Constant::Nil) => Ok(Value::Nil),
        Some(crate::undump::Constant::Number(number)) => Ok(Value::Num(*number as f32)),
        Some(crate::undump::Constant::Str(bytes)) => Ok(Value::str_from(bytes)),
        None => Err(format!("constant index {index} out of range")),
    }
}

fn arith(op: Op, lhs: &Value, rhs: &Value) -> Result<Value, String> {
    let lhs = lhs.as_num().ok_or_else(|| {
        format!(
            "attempt to perform arithmetic on a {} ({lhs:?})",
            lhs.type_name()
        )
    })?;
    let rhs = rhs.as_num().ok_or_else(|| {
        format!(
            "attempt to perform arithmetic on a {} ({rhs:?})",
            rhs.type_name()
        )
    })?;
    let result = match op {
        Op::Add => lhs + rhs,
        Op::Sub => lhs - rhs,
        Op::Mul => lhs * rhs,
        Op::Div => lhs / rhs,
        Op::Pow => lhs.powf(rhs),
        _ => unreachable!("arith called with a non-arithmetic opcode"),
    };
    Ok(Value::Num(result))
}

fn compare(op: Op, lhs: &Value, rhs: &Value) -> Result<bool, String> {
    if op == Op::Eq {
        return Ok(match (lhs, rhs) {
            (Value::Nil, Value::Nil) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Num(x), Value::Num(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Table(x), Value::Table(y)) => Rc::ptr_eq(x, y),
            (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
            _ => false,
        });
    }
    // Numeric comparison with string coercion; string ordering otherwise.
    if let (Some(x), Some(y)) = (lhs.as_num(), rhs.as_num()) {
        return Ok(if op == Op::Lt { x < y } else { x <= y });
    }
    match (lhs, rhs) {
        (Value::Str(x), Value::Str(y)) => Ok(if op == Op::Lt {
            x.as_ref() < y.as_ref()
        } else {
            x.as_ref() <= y.as_ref()
        }),
        _ => Err("attempt to compare non-comparable values".to_string()),
    }
}

fn format_number(value: f32) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

impl Vm {
    /// Test helper: calls a closure value and returns its results.
    #[cfg(test)]
    fn call_closure_for_test(&mut self, closure: Rc<LuaClosure>) -> Vec<Value> {
        self.call_closure(closure, &[])
            .expect("test closure must run")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::opcodes::{encode, encode_bx, Instruction};
    use crate::undump::{Constant, LocalVar};

    fn inst(op: Op, a: u32, b: u32, c: u32) -> Instruction {
        encode(op, a, b, c)
    }

    /// ABx instructions (LOADK/GETGLOBAL/SETGLOBAL/CLOSURE).
    fn inst_bx(op: Op, a: u32, bx: u32) -> Instruction {
        encode_bx(op, a, bx)
    }

    fn proto(constants: Vec<Constant>, code: Vec<Instruction>) -> Rc<Proto> {
        Rc::new(Proto {
            source: None,
            line_defined: 0,
            num_upvalues: 0,
            num_params: 0,
            is_vararg: 0,
            max_stack_size: 8,
            line_info: vec![],
            locals: vec![LocalVar {
                name: vec![],
                start_pc: 0,
                end_pc: 0,
            }],
            upvalues: vec![],
            constants,
            protos: vec![],
            code,
        })
    }

    #[test]
    fn multi_result_call_overwrites_callee_slot() {
        // Mirrors ydqtwo start.mr: a one-result native called with two
        // requested results must overwrite both destination slots.
        let mut vm = Vm::new();
        vm.register("_textWidth", Rc::new(|_vm, _args| Ok(vec![Value::Nil])));
        let proto = proto(
            vec![
                Constant::Str(b"_textWidth".to_vec()),
                Constant::Str(b"x".to_vec()),
                Constant::Str(b"ChWordW".to_vec()),
            ],
            vec![
                inst_bx(Op::GetGlobal, 0, 0), // r0 = _textWidth (stub)
                inst_bx(Op::LoadK, 1, 1),     // r1 = "x"
                inst(Op::Call, 0, 2, 3),      // r0, r1 = f("x") (2 results)
                inst_bx(Op::SetGlobal, 0, 2), // ChWordW = r0
                inst(Op::Return, 0, 1, 0),
            ],
        );
        vm.run_chunk(proto).expect("chunk must run");
        let value = vm.globals.borrow().get(&Value::str_from(b"ChWordW"));
        assert!(
            matches!(value, Value::Nil),
            "ChWordW must be the call result, got {value:?}"
        );
    }

    #[test]
    fn arithmetic_and_globals() {
        // answer = 6 * 7
        let proto = proto(
            vec![
                Constant::Str(b"answer".to_vec()),
                Constant::Number(6),
                Constant::Number(7),
            ],
            vec![
                inst_bx(Op::LoadK, 0, 1),     // r0 = 6
                inst_bx(Op::LoadK, 1, 2),     // r1 = 7
                inst(Op::Mul, 2, 0, 1),       // r2 = 42
                inst_bx(Op::SetGlobal, 2, 0), // answer = r2
                inst(Op::Return, 0, 1, 0),    // return
            ],
        );
        let mut vm = Vm::new();
        vm.run_chunk(proto).expect("chunk must run");
        let answer = vm.globals.borrow().get(&Value::str_from(b"answer"));
        assert_eq!(answer.as_num(), Some(42.0));
    }

    #[test]
    fn closures_share_upvalues() {
        // Tests an outer function creating a counter closure via CLOSURE
        // with a MOVE descriptor, then the inner SETting the upvalue.
        // Outer proto: LOADK 10 -> r0; CLOSURE r1 -> nested;
        //   MOVE descriptor (register 0); RETURN r1
        let nested = Rc::new(Proto {
            source: None,
            line_defined: 0,
            num_upvalues: 1,
            num_params: 1,
            is_vararg: 0,
            max_stack_size: 4,
            line_info: vec![],
            locals: vec![LocalVar {
                name: vec![],
                start_pc: 0,
                end_pc: 0,
            }],
            upvalues: vec![b"delta".to_vec()],
            constants: vec![],
            protos: vec![],
            code: vec![
                inst(Op::Add, 0, 0, 1), // r0 = r0 + upvalue... (placeholder replaced below)
                Instruction(0),         // (rewritten below)
                inst(Op::Return, 0, 2, 0),
            ],
        });
        let _ = nested;
        // Simpler, deterministic variant: verify CLOSURE descriptors bind an
        // open upvalue that both frames observe.
        let outer = Rc::new(Proto {
            source: None,
            line_defined: 0,
            num_upvalues: 0,
            num_params: 0,
            is_vararg: 0,
            max_stack_size: 8,
            line_info: vec![],
            locals: vec![LocalVar {
                name: vec![],
                start_pc: 0,
                end_pc: 0,
            }],
            upvalues: vec![],
            constants: vec![Constant::Number(5)],
            protos: vec![Proto {
                source: None,
                line_defined: 0,
                num_upvalues: 1,
                num_params: 0,
                is_vararg: 0,
                max_stack_size: 4,
                line_info: vec![],
                locals: vec![LocalVar {
                    name: vec![],
                    start_pc: 0,
                    end_pc: 0,
                }],
                upvalues: vec![b"x".to_vec()],
                constants: vec![Constant::Number(5)],
                protos: vec![],
                code: vec![
                    inst(Op::GetUpval, 0, 0, 0),
                    inst_bx(Op::LoadK, 1, 0), // r1 = 5 (constant)
                    inst(Op::Add, 0, 0, 1),
                    inst(Op::Setupval, 0, 0, 0), // x = x + 5
                    inst(Op::GetUpval, 0, 0, 0),
                    inst(Op::Return, 0, 2, 0),
                ],
            }],
            code: vec![
                inst_bx(Op::LoadK, 0, 0),               // r0 = 5
                inst_bx(Op::Closure, 1, 0),             // r1 = closure(proto0) capturing r0
                Instruction(inst(Op::Move, 0, 0, 0).0), // descriptor: capture register 0
                inst_bx(Op::SetGlobal, 1, 0),           // step = closure (global "0"? need name)
                inst(Op::Return, 0, 1, 0),
            ],
        });
        // Fix: use a proper name constant for SetGlobal.
        let mut outer2 = (*outer).clone();
        outer2.constants = vec![Constant::Number(5), Constant::Str(b"step".to_vec())];
        outer2.code[3] = inst_bx(Op::SetGlobal, 1, 1);

        let mut vm = Vm::new();
        vm.run_chunk(Rc::new(outer2)).expect("outer must run");
        // Call the global `step` twice via call path: run the same closure
        // through the VM's public API by re-registering it as a native-like
        // chunk is complex; instead call it through run_chunk on the nested.
        let closure = vm.globals.borrow().get(&Value::str_from(b"step"));
        let closure = match closure {
            Value::Closure(c) => c,
            other => panic!("expected closure global, got {other:?}"),
        };
        let first = vm.call_closure_for_test(closure.clone());
        let second = vm.call_closure_for_test(closure);
        // The closed-over cell persists between calls: 5+5=10, then
        // 10+5=15 — proof that both invocations share one upvalue.
        assert_eq!(first[0].as_num(), Some(10.0));
        assert_eq!(second[0].as_num(), Some(15.0));
    }

    #[test]
    fn tables_roundtrip_keys_and_values() {
        // t = {}; t[1] = "one"; t.x = 2
        let mut vm = Vm::new();
        let proto = proto(
            vec![
                Constant::Str(b"t".to_vec()),
                Constant::Str(b"one".to_vec()),
                Constant::Str(b"x".to_vec()),
                Constant::Number(2),
                Constant::Number(1),
            ],
            vec![
                inst(Op::NewTable, 0, 0, 0),
                inst_bx(Op::SetGlobal, 0, 0), // t = {}
                inst_bx(Op::GetGlobal, 0, 0), // r0 = t
                inst_bx(Op::LoadK, 1, 4),     // r1 = 1
                inst_bx(Op::LoadK, 2, 1),     // r2 = "one"
                inst(Op::SetTable, 0, 1, 2),  // t[1] = "one"
                inst_bx(Op::GetGlobal, 0, 0), // r0 = t
                inst_bx(Op::LoadK, 1, 2),     // r1 = "x"
                inst_bx(Op::LoadK, 2, 3),     // r2 = 2
                inst(Op::SetTable, 0, 1, 2),  // t["x"] = 2
                inst(Op::Return, 0, 1, 0),
            ],
        );
        vm.run_chunk(proto).expect("chunk must run");
        let table = vm.globals.borrow().get(&Value::str_from(b"t"));
        let table = match table {
            Value::Table(table) => table,
            other => panic!("expected table, got {other:?}"),
        };
        let borrowed = table.borrow();
        assert_eq!(borrowed.get(&Value::Num(1.0)), Value::str_from(b"one"));
        assert_eq!(borrowed.get(&Value::str_from(b"x")), Value::Num(2.0));
        assert_eq!(borrowed.len(), 1);
    }
}
