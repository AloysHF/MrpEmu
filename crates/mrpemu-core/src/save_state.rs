//! Save-state serialization for the emulator core.
//!
//! The payload is a bincode snapshot of the whole [`MrpMachine`] state,
//! compressed with LZ4 and guarded by a versioned header plus CRC32
//! checksums so corrupted or mismatched save files are rejected before use.
//! The checksum also binds the snapshot to the running package: a state
//! saved for one application refuses to load into another.
//!
//! Lua values cannot derive serde directly — tables and closures form
//! reference cycles. They travel through an index pool instead: tables and
//! closures are frozen once into `Frozen` slots and every value becomes a
//! small tagged index into that pool, which the thaw phase links back into
//! shared `Rc`s.

use bincode::Options;
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::emulator::MrpMachine;
use crate::undump::Proto;
use crate::vm::{LuaClosure, Table, Value};

/// Save-state decode/encode failures. `thiserror` keeps the core free of
/// frontend error types; frontends surface these through their own layers.
#[derive(Debug, thiserror::Error)]
pub enum SaveStateError {
    #[error("save state is truncated")]
    Truncated,
    #[error("invalid save-state signature")]
    BadSignature,
    #[error("unsupported save-state version {0}")]
    UnsupportedVersion(u32),
    #[error("save state belongs to different content data")]
    WrongContent,
    #[error("save-state checksum mismatch")]
    ChecksumMismatch,
    #[error("invalid save-state payload length")]
    BadLength,
    #[error("save-state payload exceeds the size limit")]
    TooLarge,
    #[error("save-state buffer is too small: got {got}, need {need}")]
    BufferTooSmall { got: usize, need: usize },
    #[error("failed to encode save-state payload: {0}")]
    Encode(String),
    #[error("failed to decode save state: {0}")]
    Decode(String),
    #[error("failed to decompress save state: {0}")]
    Decompress(String),
    #[error("snapshot contains cyclic Lua closures")]
    CyclicClosures,
}

type Result<T> = std::result::Result<T, SaveStateError>;

const MAGIC: &[u8; 8] = b"MRPEMST\0";
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 32;
const MAX_DECODED_SIZE: usize = 64 * 1024 * 1024;

/// Fixed capacity required by the libretro serialization API. Guest memory
/// dominates the payload; LZ4 keeps typical snapshots far below the cap.
pub const SERIALIZED_SIZE: usize = 64 * 1024 * 1024;

/// Encodes a machine snapshot into the fixed-size save-state buffer,
/// binding it to the running package via its content checksum.
pub fn encode_machine(machine: &MrpMachine, output: &mut [u8]) -> Result<()> {
    encode_value(machine, machine.package_crc32(), output)
}

/// Decodes a machine snapshot, rejecting states from other content builds.
/// Restores the native-function registries and the shared globals table
/// that cannot travel through serde.
pub fn decode_machine(input: &[u8], expected_content_crc32: u32) -> Result<MrpMachine> {
    let decoded = decode_payload(input, expected_content_crc32)?;
    let mut machine: MrpMachine = decode_value(&decoded)?;
    machine.rebuild_snapshot_state();
    Ok(machine)
}

/// Returns the content checksum a snapshot must have been saved with for
/// this machine's package.
pub fn content_crc32(machine: &MrpMachine) -> u32 {
    machine.package_crc32()
}

pub(crate) fn encode_value<T: Serialize>(
    value: &T,
    content_crc32: u32,
    output: &mut [u8],
) -> Result<()> {
    if output.len() < SERIALIZED_SIZE {
        return Err(SaveStateError::BufferTooSmall {
            got: output.len(),
            need: SERIALIZED_SIZE,
        });
    }

    let decoded = codec()
        .serialize(value)
        .map_err(|error| SaveStateError::Encode(error.to_string()))?;
    if decoded.len() > MAX_DECODED_SIZE {
        return Err(SaveStateError::TooLarge);
    }

    let payload = lz4_flex::compress(&decoded);
    if payload.len() > SERIALIZED_SIZE - HEADER_SIZE {
        return Err(SaveStateError::TooLarge);
    }

    output.fill(0);
    output[..8].copy_from_slice(MAGIC);
    output[8..12].copy_from_slice(&VERSION.to_le_bytes());
    output[12..16].copy_from_slice(&content_crc32.to_le_bytes());
    output[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    output[20..24].copy_from_slice(&(decoded.len() as u32).to_le_bytes());
    output[24..28].copy_from_slice(&crc32fast::hash(&payload).to_le_bytes());
    output[HEADER_SIZE..HEADER_SIZE + payload.len()].copy_from_slice(&payload);
    Ok(())
}

pub(crate) fn decode_payload(input: &[u8], expected_content_crc32: u32) -> Result<Vec<u8>> {
    if input.len() < HEADER_SIZE {
        return Err(SaveStateError::Truncated);
    }
    if &input[..8] != MAGIC {
        return Err(SaveStateError::BadSignature);
    }

    let version = read_u32(input, 8);
    if version != VERSION {
        return Err(SaveStateError::UnsupportedVersion(version));
    }
    if read_u32(input, 12) != expected_content_crc32 {
        return Err(SaveStateError::WrongContent);
    }

    let payload_len = read_u32(input, 16) as usize;
    let decoded_len = read_u32(input, 20) as usize;
    if decoded_len > MAX_DECODED_SIZE {
        return Err(SaveStateError::TooLarge);
    }

    let payload_end = HEADER_SIZE
        .checked_add(payload_len)
        .filter(|&end| end <= input.len() && end <= SERIALIZED_SIZE)
        .ok_or(SaveStateError::BadLength)?;
    let payload = &input[HEADER_SIZE..payload_end];
    if crc32fast::hash(payload) != read_u32(input, 24) {
        return Err(SaveStateError::ChecksumMismatch);
    }

    lz4_flex::decompress(payload, decoded_len)
        .map_err(|error| SaveStateError::Decompress(error.to_string()))
}

/// Deserializes a value from an uncompressed snapshot payload (tests and
/// machine-level restore reuse the same codec settings).
pub(crate) fn decode_value<T: DeserializeOwned>(decoded: &[u8]) -> Result<T> {
    codec()
        .with_limit(MAX_DECODED_SIZE as u64)
        .deserialize(decoded)
        .map_err(|error| SaveStateError::Decode(error.to_string()))
}

fn codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
}

fn read_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// Instant serde helpers. `Instant` is neither serializable nor meaningful
// across processes, so timestamps travel as durations relative to "now":
// past instants (guest start) as elapsed uptime, future instants (timer
// deadlines) as remaining time that re-anchors to the load moment.
// ---------------------------------------------------------------------------

pub(crate) fn ser_instant_elapsed<S: Serializer>(
    instant: &Instant,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    instant.elapsed().serialize(serializer)
}

pub(crate) fn de_instant_elapsed<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Instant, D::Error> {
    let elapsed = Duration::deserialize(deserializer)?;
    Ok(Instant::now()
        .checked_sub(elapsed)
        .unwrap_or_else(Instant::now))
}

pub(crate) fn ser_instant_remaining<S: Serializer>(
    instant: &Instant,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    let remaining = instant
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO);
    remaining.serialize(serializer)
}

pub(crate) fn de_instant_remaining<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Instant, D::Error> {
    let remaining = Duration::deserialize(deserializer)?;
    Ok(Instant::now() + remaining)
}

// ---------------------------------------------------------------------------
// Lua value pool. Tables, closures and upvalues form reference cycles, so
// freeze assigns every table/closure an index and turns each value into a
// tagged index; thaw rebuilds the shared `Rc`s from the pool.
// ---------------------------------------------------------------------------

/// A frozen Lua value: either plain data or a pool index.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum FrozenValue {
    Nil,
    Bool(bool),
    Num(f32),
    Bytes(Vec<u8>),
    Table(u32),
    Closure(u32),
    Native(u16),
}

/// A frozen table key. Table/closure keys travel as pool indices and are
/// re-anchored to the restored object addresses on thaw.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum FrozenKey {
    Bool(bool),
    Num(u32),
    Bytes(Vec<u8>),
    Table(u32),
    Closure(u32),
    Native(u16),
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub(crate) struct FrozenTable {
    array: Vec<FrozenValue>,
    map: Vec<(FrozenKey, FrozenValue)>,
    metatable: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct FrozenClosure {
    proto: Rc<Proto>,
    upvals: Vec<FrozenValue>,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Frozen {
    tables: Vec<FrozenTable>,
    closures: Vec<FrozenClosure>,
}

/// Walks a live Lua value graph into the frozen pool, preserving object
/// identity (a shared table freezes once).
#[derive(Default)]
pub(crate) struct Freezer {
    frozen: Frozen,
    table_ids: HashMap<usize, u32>,
    closure_ids: HashMap<usize, u32>,
}

impl Freezer {
    pub(crate) fn value(&mut self, value: &Value) -> FrozenValue {
        match value {
            Value::Nil => FrozenValue::Nil,
            Value::Bool(flag) => FrozenValue::Bool(*flag),
            Value::Num(number) => FrozenValue::Num(*number),
            Value::Str(bytes) => FrozenValue::Bytes((**bytes).clone()),
            Value::Table(table) => FrozenValue::Table(self.table_id(table)),
            Value::Closure(closure) => FrozenValue::Closure(self.closure_id(closure)),
            Value::Native(index) => FrozenValue::Native(*index),
        }
    }

    pub(crate) fn table_id(&mut self, table: &Rc<std::cell::RefCell<Table>>) -> u32 {
        let address = Rc::as_ptr(table) as *const u8 as usize;
        if let Some(id) = self.table_ids.get(&address) {
            return *id;
        }
        let id = self.frozen.tables.len() as u32;
        self.table_ids.insert(address, id);
        self.frozen.tables.push(FrozenTable::default());

        let table = table.borrow();
        let array: Vec<FrozenValue> = table.array.iter().map(|v| self.value(v)).collect();
        let mut map = Vec::with_capacity(table.map.len());
        for (key, entry) in &table.map {
            if let Some(frozen_key) = self.key(key) {
                map.push((frozen_key, self.value(entry)));
            }
        }
        let metatable = table.metatable.as_ref().map(|meta| self.table_id(meta));
        let slot = &mut self.frozen.tables[id as usize];
        slot.array = array;
        slot.map = map;
        slot.metatable = metatable;
        id
    }

    fn closure_id(&mut self, closure: &Rc<LuaClosure>) -> u32 {
        let address = Rc::as_ptr(closure) as *const u8 as usize;
        if let Some(id) = self.closure_ids.get(&address) {
            return *id;
        }
        let id = self.frozen.closures.len() as u32;
        self.closure_ids.insert(address, id);
        let upvals: Vec<FrozenValue> = closure
            .upvals
            .iter()
            .map(|upval| self.value(&upval.borrow()))
            .collect();
        let proto = Rc::clone(&closure.proto);
        self.frozen.closures.push(FrozenClosure { proto, upvals });
        id
    }

    fn key(&mut self, key: &crate::vm::Key) -> Option<FrozenKey> {
        use crate::vm::Key;
        Some(match key {
            Key::Bool(flag) => FrozenKey::Bool(*flag),
            Key::Num(bits) => FrozenKey::Num(*bits),
            Key::Str(bytes) => FrozenKey::Bytes(bytes.clone()),
            Key::Table(address) => FrozenKey::Table(*self.table_ids.get(address)?),
            Key::Closure(address) => FrozenKey::Closure(*self.closure_ids.get(address)?),
            Key::Native(index) => FrozenKey::Native(*index),
        })
    }

    pub(crate) fn into_frozen(self) -> Frozen {
        self.frozen
    }
}

/// Rebuilds live Lua values from the frozen pool. Table shells exist before
/// their content fills (tables are behind `RefCell`, so cycles resolve);
/// closures fill lazily in dependency order and reject closure cycles.
pub(crate) struct Thaw {
    frozen: Frozen,
    tables: Vec<Rc<std::cell::RefCell<Table>>>,
    closures: Vec<Option<Rc<LuaClosure>>>,
    filling: Vec<bool>,
    /// Table ids whose content fill is in progress; a self/mutual reference
    /// met during the fill returns the shared shell, which the outer call
    /// completes.
    filling_tables: std::collections::HashSet<u32>,
}

impl Thaw {
    pub(crate) fn new(frozen: Frozen) -> Self {
        let tables = frozen
            .tables
            .iter()
            .map(|_| Rc::new(std::cell::RefCell::new(Table::default())))
            .collect();
        let count = frozen.closures.len();
        Self {
            frozen,
            tables,
            closures: (0..count).map(|_| None).collect(),
            filling: vec![false; count],
            filling_tables: std::collections::HashSet::new(),
        }
    }

    pub(crate) fn value(&mut self, frozen_value: &FrozenValue) -> Result<Value> {
        Ok(match frozen_value {
            FrozenValue::Nil => Value::Nil,
            FrozenValue::Bool(flag) => Value::Bool(*flag),
            FrozenValue::Num(number) => Value::Num(*number),
            FrozenValue::Bytes(bytes) => Value::str_from(bytes),
            FrozenValue::Table(id) => Value::Table(self.table(*id)?),
            FrozenValue::Closure(id) => Value::Closure(self.closure(*id)?),
            FrozenValue::Native(index) => Value::Native(*index),
        })
    }

    pub(crate) fn table(&mut self, id: u32) -> Result<Rc<std::cell::RefCell<Table>>> {
        let shell = self
            .tables
            .get(id as usize)
            .cloned()
            .ok_or(SaveStateError::Decode(
                "frozen table index out of range".into(),
            ))?;
        // A reference met while this table's own fill is in progress points
        // at the same shell; the outer call finishes the content.
        if self.filling_tables.contains(&id) {
            return Ok(shell);
        }
        // Clone the frozen slot so recursive value thawing can borrow self.
        let frozen_table = self.frozen.tables[id as usize].clone();
        // Tables sit behind RefCells, so content can fill after the shell is
        // shared; re-visiting a shared table finds it already filled and
        // returns the same object.
        let already_filled = {
            let table = shell.borrow();
            !table.array.is_empty() || !table.map.is_empty() || table.metatable.is_some()
        };
        if already_filled {
            return Ok(shell);
        }
        self.filling_tables.insert(id);
        {
            let mut table = shell.borrow_mut();
            table.array = frozen_table
                .array
                .iter()
                .map(|fv| self.value(fv))
                .collect::<Result<_>>()?;
            for (frozen_key, frozen_entry) in &frozen_table.map {
                let key = self.key(frozen_key)?;
                let entry = self.value(frozen_entry)?;
                table.map.insert(key, entry);
            }
            table.metatable = frozen_table
                .metatable
                .map(|meta| self.table(meta))
                .transpose()?;
        }
        self.filling_tables.remove(&id);
        Ok(shell)
    }

    fn key(&mut self, frozen_key: &FrozenKey) -> Result<crate::vm::Key> {
        use crate::vm::Key;
        Ok(match frozen_key {
            FrozenKey::Bool(flag) => Key::Bool(*flag),
            FrozenKey::Num(bits) => Key::Num(*bits),
            FrozenKey::Bytes(bytes) => Key::Str(bytes.clone()),
            FrozenKey::Table(id) => {
                let shell = self.table(*id)?;
                Key::Table(Rc::as_ptr(&shell) as *const u8 as usize)
            }
            FrozenKey::Closure(id) => {
                let closure = self.closure(*id)?;
                Key::Closure(Rc::as_ptr(&closure) as *const u8 as usize)
            }
            FrozenKey::Native(index) => Key::Native(*index),
        })
    }

    fn closure(&mut self, id: u32) -> Result<Rc<LuaClosure>> {
        let index = id as usize;
        if let Some(closure) = &self.closures[index] {
            return Ok(Rc::clone(closure));
        }
        if self.filling[index] {
            return Err(SaveStateError::CyclicClosures);
        }
        self.filling[index] = true;
        let frozen_upvals = self.frozen.closures[index].upvals.clone();
        let upvals = frozen_upvals
            .iter()
            .map(|fv| {
                self.value(fv)
                    .map(|value| Rc::new(std::cell::RefCell::new(value)))
            })
            .collect::<Result<Vec<_>>>()?;
        let closure = Rc::new(LuaClosure {
            proto: Rc::clone(&self.frozen.closures[index].proto),
            upvals,
        });
        self.closures[index] = Some(Rc::clone(&closure));
        self.filling[index] = false;
        Ok(closure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A self-referencing table (t.field = t) must round-trip through the
    /// pool and come back as a single shared object.
    #[test]
    fn cyclic_table_roundtrips_as_one_object() -> Result<()> {
        let shared = Rc::new(RefCell::new(Table::default()));
        shared.borrow_mut().array.push(Value::Num(7.0));
        shared.borrow_mut().map.insert(
            crate::vm::Key::Str(b"self".to_vec()),
            Value::Table(Rc::clone(&shared)),
        );

        let mut freezer = Freezer::default();
        let frozen_root = freezer.value(&Value::Table(Rc::clone(&shared)));
        let frozen = freezer.into_frozen();

        let mut thaw = Thaw::new(frozen);
        let restored = thaw.value(&frozen_root)?;
        let Value::Table(restored_table) = &restored else {
            panic!("expected a table");
        };
        let inner = restored_table.borrow();
        assert_eq!(inner.array, vec![Value::Num(7.0)]);
        match inner.map.get(&crate::vm::Key::Str(b"self".to_vec())) {
            Some(Value::Table(back)) => {
                assert!(Rc::ptr_eq(back, restored_table), "cycle must be shared");
            }
            other => panic!("expected the self reference, got {other:?}"),
        }
        Ok(())
    }

    /// Two tables sharing one metatable come back sharing one metatable.
    #[test]
    fn shared_metatable_roundtrips_shared() -> Result<()> {
        let meta = Rc::new(RefCell::new(Table::default()));
        meta.borrow_mut().array.push(Value::Bool(true));
        let make_table = || {
            let table = Rc::new(RefCell::new(Table::default()));
            table.borrow_mut().metatable = Some(Rc::clone(&meta));
            table
        };
        let first = make_table();
        let second = make_table();

        let mut freezer = Freezer::default();
        let first_root = freezer.value(&Value::Table(first));
        let second_root = freezer.value(&Value::Table(second));
        let frozen = freezer.into_frozen();

        let mut thaw = Thaw::new(frozen);
        let Value::Table(restored_first) = thaw.value(&first_root)? else {
            panic!("expected a table");
        };
        let Value::Table(restored_second) = thaw.value(&second_root)? else {
            panic!("expected a table");
        };
        let first_meta = restored_first.borrow().metatable.clone();
        let second_meta = restored_second.borrow().metatable.clone();
        assert!(Rc::ptr_eq(&first_meta.unwrap(), &second_meta.unwrap()));
        Ok(())
    }
}

#[cfg(test)]
mod machine_tests {
    use super::*;
    use crate::emulator::MrpMachine;

    /// Builds a minimal structurally valid package: header + info block and
    /// a file list that stops immediately, so boot fails cleanly but the
    /// machine, header, and package stay fully populated for a round-trip.
    fn minimal_package() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(b"MRPG");
        data.extend_from_slice(&200u32.to_le_bytes()); // info size
        data.extend_from_slice(&216u32.to_le_bytes()); // file size
        data.extend_from_slice(&216u32.to_le_bytes()); // list offset
        data.extend_from_slice(&[0u8; 208]);
        data
    }

    #[test]
    fn machine_shell_roundtrip_without_boot() {
        let data = minimal_package();
        let path =
            std::env::temp_dir().join(format!("mrpemu-snapshot-shell-{}.mrp", std::process::id()));
        std::fs::write(&path, &data).expect("write package");
        let mut machine = MrpMachine::from_path(&path).expect("parse");
        machine.run_frame();
        let _ = std::fs::remove_file(&path);

        let mut buffer = vec![0u8; SERIALIZED_SIZE];
        encode_machine(&machine, &mut buffer).expect("encode");
        let restored = decode_machine(&buffer, content_crc32(&machine)).expect("decode");
        assert_eq!(
            restored.boot_error(),
            machine.boot_error(),
            "boot failure state round-trips"
        );
        assert_eq!(
            restored.header().display_name,
            machine.header().display_name
        );

        // A snapshot bound to other content must be rejected.
        assert!(decode_machine(&buffer, 0xDEAD_BEEF).is_err());
        // Corrupted payload rejected by the checksum.
        let mut corrupted = buffer.clone();
        corrupted[HEADER_SIZE] ^= 0xFF;
        assert!(decode_machine(&corrupted, content_crc32(&machine)).is_err());
        // Wrong signature rejected.
        let mut foreign = buffer.clone();
        foreign[..8].copy_from_slice(b"XXXXXXXX");
        assert!(decode_machine(&foreign, content_crc32(&machine)).is_err());
    }
}
