//! Small conversions shared by the script natives.

use std::cell::RefCell;
use std::rc::Rc;

use crate::vm::{Key, Table, Value};

pub fn str_value(bytes: &[u8]) -> Value {
    Value::str_from(bytes)
}

pub fn num_value(value: f32) -> Value {
    Value::Num(value)
}

pub fn as_num(value: &Value) -> Option<f32> {
    value.as_num()
}

pub fn to_string_bytes(value: &Value) -> Vec<u8> {
    match value {
        Value::Str(text) => text.as_ref().to_vec(),
        Value::Num(number) => {
            if number.fract() == 0.0 && number.abs() < 1e15 {
                format!("{}", *number as i64).into_bytes()
            } else {
                format!("{number}").into_bytes()
            }
        }
        Value::Bool(flag) => {
            if *flag {
                b"true".to_vec()
            } else {
                b"false".to_vec()
            }
        }
        _ => b"nil".to_vec(),
    }
}

/// Builds a table through the callback and wraps it.
pub fn table_value(build: impl FnOnce(&mut Table)) -> Value {
    let mut table = Table::default();
    build(&mut table);
    Value::Table(Rc::new(RefCell::new(table)))
}

pub fn key_to_value(key: &Key) -> Value {
    match key {
        Key::Bool(flag) => Value::Bool(*flag),
        Key::Num(bits) => Value::Num(f32::from_bits(*bits)),
        Key::Str(bytes) => Value::str_from(bytes),
        Key::Table(id) => {
            let _ = id;
            Value::Nil // identity keys are not reconstructible from the map
        }
        Key::Closure(_) | Key::Native(_) => Value::Nil,
    }
}

pub fn values_equal(lhs: &Value, rhs: &Value) -> bool {
    lhs == rhs
}
