//! The Lua C API compatibility layer over the MiniJ VM value model.
//!
//! SDK wrapper exts drive a Lua state through `_mr_c_internal_table`
//! entries 16-76 (the `asm_mrp_*` wrappers in the reference runtime): they
//! read `vm_state` from entry 1, register game functions with
//! `pushcclosure`/`settable`, load embedded bytecode chunks with `load`,
//! and call handlers with `call`. The state here is host-side: a value
//! stack, the globals table, and a MiniJ VM for executing bytecode
//! closures.
//!
//! Guest C closures are represented as [`Value::Native`] indices at or
//! above [`CCLOSURE_BASE`]; the guest function pointer lives in
//! [`LuaState::cclosures`]. A direct `mrp_call` on such a value is
//! resolved by the caller (the bridge dispatch) without the VM.

use std::cell::RefCell;
use std::rc::Rc;

use crate::vm::{Table, Value};

/// Pseudo-index of the globals table (`MRP_GLOBALSINDEX`).
pub const GLOBALS_INDEX: i32 = -10001;
/// Pseudo-index of the registry table (`MRP_REGISTRYINDEX`).
pub const REGISTRY_INDEX: i32 = -10000;

/// Lua type codes exposed through `mrp_type`/`mrp_typename`.
pub const T_NONE: i32 = -1;
pub const T_NIL: i32 = 0;
pub const T_BOOLEAN: i32 = 1;
pub const T_LIGHTUSERDATA: i32 = 2;
pub const T_NUMBER: i32 = 3;
pub const T_STRING: i32 = 4;
pub const T_TABLE: i32 = 5;
pub const T_FUNCTION: i32 = 6;
pub const T_USERDATA: i32 = 7;

/// Native-index threshold marking guest C closures: `Value::Native(i)` with
/// `i >= CCLOSURE_BASE` addresses [`LuaState::cclosures`]. Regular VM
/// natives live below it.
pub const CCLOSURE_BASE: u16 = 0x8000;

/// One host-side Lua state (`mrp_State`).
pub struct LuaState {
    /// The C API value stack.
    pub stack: Vec<Value>,
    /// The globals table, shared with the VM that runs bytecode chunks.
    pub globals: Rc<RefCell<Table>>,
    /// Guest C closure function pointers; `Value::Native(CCLOSURE_BASE + i)`
    /// references entry `i`.
    pub cclosures: Vec<u32>,
    /// Absolute stack base of each active C-call frame (negative-index
    /// resolution inside a C function is relative to its frame).
    pub frames: Vec<usize>,
    /// Guest light-userdata pointers, mirrored as [`Value::Native`]
    /// indices at or above [`LIGHTUSERDATA_BASE`].
    pub lightuserdata: Vec<u32>,
    /// Guest userdata blocks, mirrored as indices at or above
    /// [`USERDATA_BASE`].
    pub userdata: Vec<(u32, u32)>,
    /// The last error message (`mrp_error`/pcall channel).
    pub error: Option<String>,
}

/// Native-index threshold marking guest light userdata values.
pub const LIGHTUSERDATA_BASE: u16 = 0xC000;
/// Native-index threshold marking guest userdata values.
pub const USERDATA_BASE: u16 = 0xA000;

impl Default for LuaState {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaState {
    /// Creates an empty state.
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            globals: Rc::new(RefCell::new(Table::default())),
            cclosures: Vec::new(),
            frames: Vec::new(),
            lightuserdata: Vec::new(),
            userdata: Vec::new(),
            error: None,
        }
    }

    /// Resolves a C API stack index to an absolute stack position.
    /// Positive indices are 1-based from the current frame base; negative
    /// indices count from the top; pseudo-indices are rejected here (the
    /// table ops handle them separately).
    pub fn abs_index(&self, idx: i32) -> Option<usize> {
        let base = self.frames.last().copied().unwrap_or(0);
        if idx > 0 {
            let pos = base + (idx as usize).saturating_sub(1);
            (pos < self.stack.len()).then_some(pos)
        } else if idx < 0 && -idx <= self.stack.len() as i32 {
            Some((self.stack.len() as i32 + idx) as usize)
        } else {
            None
        }
    }

    /// The Lua type code of the value at `idx` (pseudo-index aware).
    pub fn type_at(&self, idx: i32) -> i32 {
        match idx {
            GLOBALS_INDEX | REGISTRY_INDEX => T_TABLE,
            i if i < 0 => match self.abs_index(i) {
                Some(pos) => type_code(&self.stack[pos]),
                None => T_NONE,
            },
            i => match self.abs_index(i) {
                Some(pos) => type_code(&self.stack[pos]),
                None => T_NONE,
            },
        }
    }

    /// Pushes a value onto the stack.
    pub fn push(&mut self, value: Value) {
        self.stack.push(value);
    }

    /// Returns (without removing) the value at `idx`; pseudo-indices yield
    /// the corresponding host table as a table value.
    pub fn peek(&self, idx: i32) -> Option<Value> {
        match idx {
            GLOBALS_INDEX => Some(Value::Table(Rc::clone(&self.globals))),
            REGISTRY_INDEX => None,
            _ => self.abs_index(idx).map(|pos| self.stack[pos].clone()),
        }
    }

    /// Pops the top value.
    pub fn pop(&mut self) -> Value {
        self.stack.pop().unwrap_or(Value::Nil)
    }

    /// Sets the stack top, mirroring `mrp_settop` semantics (a positive
    /// count pads with nils, a negative delta pops).
    pub fn set_top(&mut self, idx: i32) {
        match idx {
            i if i >= 0 => {
                let base = self.frames.last().copied().unwrap_or(0);
                let target = base + idx as usize;
                while self.stack.len() < target {
                    self.stack.push(Value::Nil);
                }
                self.stack.truncate(target);
            }
            i => {
                let target = (self.stack.len() as i32 + i).max(0) as usize;
                self.stack.truncate(target);
            }
        }
    }

    /// Registers a guest C closure and pushes its value. Returns the
    /// `Value::Native` encoding.
    pub fn push_cclosure(&mut self, fn_ptr: u32, _nups: i32) -> Value {
        let index = self.cclosures.len() as u16;
        self.cclosures.push(fn_ptr);
        let value = Value::Native(CCLOSURE_BASE + index);
        self.stack.push(value.clone());
        value
    }

    /// Pushes a guest pointer as light userdata.
    pub fn push_lightuserdata(&mut self, ptr: u32) -> Value {
        let index = self.lightuserdata.len() as u16;
        self.lightuserdata.push(ptr);
        let value = Value::Native(LIGHTUSERDATA_BASE + index);
        self.stack.push(value.clone());
        value
    }

    /// Allocates a guest userdata block (host-tracked size, zeroed guest
    /// memory is the caller's responsibility) and pushes its value.
    pub fn new_userdata(&mut self, ptr: u32, size: u32) -> Value {
        let index = self.userdata.len() as u16;
        self.userdata.push((ptr, size));
        let value = Value::Native(USERDATA_BASE + index);
        self.stack.push(value.clone());
        value
    }

    /// The guest pointer behind a light-userdata / userdata value, if any.
    pub fn userdata_ptr(&self, value: &Value) -> Option<u32> {
        match value {
            Value::Native(i) if *i >= LIGHTUSERDATA_BASE => self
                .lightuserdata
                .get((*i - LIGHTUSERDATA_BASE) as usize)
                .copied(),
            Value::Native(i) if *i >= USERDATA_BASE => self
                .userdata
                .get((*i - USERDATA_BASE) as usize)
                .map(|(ptr, _)| *ptr),
            _ => None,
        }
    }

    /// The guest function pointer behind a C closure value, if any.
    pub fn cclosure_ptr(&self, value: &Value) -> Option<u32> {
        match value {
            Value::Native(i) if (*i >= CCLOSURE_BASE) && (*i < USERDATA_BASE) => {
                self.cclosures.get((*i - CCLOSURE_BASE) as usize).copied()
            }
            _ => None,
        }
    }

    /// `mrp_settable`: pops `value` then `key` and stores into the table at
    /// `idx` (pseudo-index aware).
    pub fn set_table(&mut self, idx: i32) {
        let value = self.pop();
        let key = self.pop();
        if let Some(table) = self.table_at(idx) {
            table.borrow_mut().set(key, value);
        }
    }

    /// `mrp_rawset`: same as [`Self::set_table`] (metatables are not
    /// consulted here anyway).
    pub fn raw_set(&mut self, idx: i32) {
        self.set_table(idx);
    }

    /// `mrp_rawseti`: sets `t[n] = value` (the value is popped).
    pub fn raw_seti(&mut self, idx: i32, n: i32) {
        let value = self.pop();
        if let Some(table) = self.table_at(idx) {
            table.borrow_mut().set(Value::Num(n as f32), value);
        }
    }

    /// `mrp_gettable`: pops the key and pushes `table[key]` (pseudo-index
    /// aware).
    pub fn get_table(&mut self, idx: i32) {
        let key = self.pop();
        let result = match self.table_at(idx) {
            Some(table) => table.borrow().get(&key),
            None => Value::Nil,
        };
        self.push(result);
    }

    /// `mrp_rawget`: same lookup without metamethods.
    pub fn raw_get(&mut self, idx: i32) {
        self.get_table(idx);
    }

    /// `mrp_rawgeti`: pushes `t[n]`.
    pub fn raw_geti(&mut self, idx: i32, n: i32) {
        let result = match self.table_at(idx) {
            Some(table) => table.borrow().get(&Value::Num(n as f32)),
            None => Value::Nil,
        };
        self.push(result);
    }

    /// The host table behind a stack position or pseudo-index.
    fn table_at(&self, idx: i32) -> Option<Rc<RefCell<Table>>> {
        match self.peek(idx)? {
            Value::Table(table) => Some(table),
            _ => None,
        }
    }
}

/// The Lua type code of a host value.
pub fn type_code(value: &Value) -> i32 {
    match value {
        Value::Nil => T_NIL,
        Value::Bool(_) => T_BOOLEAN,
        Value::Num(_) => T_NUMBER,
        Value::Str(_) => T_STRING,
        Value::Table(_) => T_TABLE,
        Value::Closure(_) => T_FUNCTION,
        // Native indices encode the guest-backed kinds by range: platform
        // natives and C closures are functions, userdata/userdata-pointer
        // ranges surface as their Lua kinds.
        Value::Native(i) => {
            if *i >= LIGHTUSERDATA_BASE {
                T_LIGHTUSERDATA
            } else if *i >= USERDATA_BASE {
                T_USERDATA
            } else {
                T_FUNCTION
            }
        }
    }
}

/// The Lua type name for a type code (the `mrp_typename` strings).
pub fn type_name(code: i32) -> &'static str {
    match code {
        T_NONE => "no value",
        T_NIL => "nil",
        T_BOOLEAN => "boolean",
        T_LIGHTUSERDATA => "userdata",
        T_NUMBER => "number",
        T_STRING => "string",
        T_TABLE => "table",
        T_FUNCTION => "function",
        T_USERDATA => "userdata",
        _ => "no value",
    }
}

/// The short type name (`mrp_shorttypename`): the first three letters.
pub fn short_type_name(code: i32) -> &'static str {
    match code {
        T_NONE => "no ",
        T_NIL => "nil",
        T_BOOLEAN => "boo",
        T_LIGHTUSERDATA => "uda",
        T_NUMBER => "num",
        T_STRING => "str",
        T_TABLE => "tab",
        T_FUNCTION => "fun",
        T_USERDATA => "uda",
        _ => "no ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stack indices resolve per Lua convention.
    #[test]
    fn abs_index_resolution() {
        let mut state = LuaState::new();
        state.push(Value::Num(1.0));
        state.push(Value::Num(2.0));
        state.push(Value::Num(3.0));
        assert_eq!(state.abs_index(1), Some(0));
        assert_eq!(state.abs_index(3), Some(2));
        assert_eq!(state.abs_index(-1), Some(2));
        assert_eq!(state.abs_index(-3), Some(0));
        assert_eq!(state.abs_index(4), None);
        assert_eq!(state.abs_index(-4), None);
    }

    /// C-call frames shift the positive-index base.
    #[test]
    fn frame_bases() {
        let mut state = LuaState::new();
        state.push(Value::Num(9.0)); // outer value
        state.frames.push(state.stack.len());
        state.push(Value::Num(1.0));
        state.push(Value::Num(2.0));
        assert_eq!(state.abs_index(1), Some(1));
        assert_eq!(state.abs_index(2), Some(2));
        assert_eq!(state.abs_index(-1), Some(2));
        state.frames.pop();
        assert_eq!(state.abs_index(1), Some(0));
    }

    /// Globals pseudo-index stores and reads through settable/gettable.
    #[test]
    fn globals_pseudo_index() {
        let mut state = LuaState::new();
        state.push(Value::str_from(b"score"));
        state.push(Value::Num(42.0));
        state.set_table(GLOBALS_INDEX);
        state.push(Value::str_from(b"score"));
        state.get_table(GLOBALS_INDEX);
        assert_eq!(state.pop(), Value::Num(42.0));
    }

    /// Raw indexed access round-trips.
    #[test]
    fn raw_get_set_i() {
        let mut state = LuaState::new();
        state.push(Value::Table(Rc::new(RefCell::new(Table::default()))));
        state.push(Value::Num(7.0));
        state.raw_seti(1, 3);
        state.raw_geti(1, 3);
        assert_eq!(state.pop(), Value::Num(7.0));
        state.set_top(0);
        assert!(state.stack.is_empty());
    }

    /// Closures and userdata values decode back to guest pointers.
    #[test]
    fn pointer_decoding() {
        let mut state = LuaState::new();
        let closure = state.push_cclosure(0x1234_5678, 0);
        assert_eq!(state.cclosure_ptr(&closure), Some(0x1234_5678));
        state.pop();
        let light = state.push_lightuserdata(0x1000);
        assert_eq!(state.userdata_ptr(&light), Some(0x1000));
        assert_eq!(type_code(&light), T_LIGHTUSERDATA);
        state.pop();
    }

    /// Type codes map to the reference names.
    #[test]
    fn type_names() {
        assert_eq!(type_name(T_NIL), "nil");
        assert_eq!(type_name(T_FUNCTION), "function");
        assert_eq!(short_type_name(T_TABLE), "tab");
    }
}
