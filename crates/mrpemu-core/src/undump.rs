//! Deserializer for MiniJ bytecode chunks (`\x1bMRP` compiled scripts).
//!
//! MiniJ chunks are Lua 5.0 compiled chunks with a stripped header: the
//! four-byte signature `\x1bMRP` is followed by one version byte (`0x80`)
//! and one endianness byte (`1` = little endian), then immediately the main
//! function prototype. All integers are little-endian `u32`, numbers are
//! little-endian `f32`, and strings are a `u32` byte length (including the
//! trailing NUL the writer appends) followed by that many bytes.
//!
//! Field order per prototype (verified against every sample corpus member:
//! consumed byte count must equal the chunk length):
//! source string, line-defined `u32`, then four bytes (num upvalues, num
//! params, is-vararg, max stack size), then line info, locals, upvalues,
//! constants, nested prototypes, and finally the instruction code.

use crate::error::MrpError;
use crate::opcodes::{Instruction, OP_RETURN};

/// The four-byte chunk signature (`ESC` + `MRP`).
pub const CHUNK_MAGIC: [u8; 4] = [0x1B, b'M', b'R', b'P'];

/// Guard against corrupt or malicious nesting depth.
const MAX_PROTO_DEPTH: usize = 64;

/// A constant table entry.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Constant {
    Nil,
    /// MiniJ stores `lua_Number` as 32-bit integers (feature-phone Lua).
    Number(i32),
    /// Byte string; MRP scripts encode text in GBK, so this stays raw.
    Str(Vec<u8>),
}

/// A debug-entry local variable record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalVar {
    pub name: Vec<u8>,
    pub start_pc: u32,
    pub end_pc: u32,
}

/// One function prototype inside a chunk.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Proto {
    pub source: Option<Vec<u8>>,
    pub line_defined: u32,
    pub num_upvalues: u8,
    pub num_params: u8,
    pub is_vararg: u8,
    pub max_stack_size: u8,
    pub line_info: Vec<u32>,
    pub locals: Vec<LocalVar>,
    pub upvalues: Vec<Vec<u8>>,
    pub constants: Vec<Constant>,
    pub protos: Vec<Proto>,
    pub code: Vec<Instruction>,
}

impl Proto {
    /// Whether every instruction decodes to a known opcode and the final
    /// instruction is a return, mirroring the runtime's own chunk check.
    pub fn code_is_plausible(&self) -> bool {
        if self.code.is_empty() {
            return false;
        }
        if self.code.last().expect("checked non-empty").op() != OP_RETURN {
            return false;
        }
        self.code
            .iter()
            .all(|inst| crate::opcodes::Op::from_u8(inst.op()).is_some())
    }
}

/// A parsed `\x1bMRP` chunk: the main prototype plus bookkeeping.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub proto: Proto,
}

impl Chunk {
    /// Parses a full chunk from `data`; parsing must consume every byte.
    pub fn parse(data: &[u8]) -> Result<Self, MrpError> {
        if data.len() < 6 {
            return Err(MrpError::TooSmall {
                expected: 6,
                got: data.len(),
            });
        }
        if data[0..4] != CHUNK_MAGIC {
            return Err(MrpError::BadMagic {
                found: [data[0], data[1], data[2], data[3]],
            });
        }
        // data[4] = version (0x80 on the sample corpus); data[5] = endianness.
        if data[5] != 1 {
            return Err(MrpError::Unsupported(format!(
                "big-endian MiniJ chunks are not supported (endian byte {})",
                data[5]
            )));
        }

        let mut reader = Reader {
            data,
            pos: 6,
            depth: 0,
        };
        let proto = reader.read_proto()?;
        if reader.pos != data.len() {
            return Err(MrpError::Unsupported(format!(
                "chunk has {} trailing bytes after the main prototype",
                data.len() - reader.pos
            )));
        }
        Ok(Self { proto })
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Reader<'a> {
    fn need(&self, size: usize, field: &'static str) -> Result<(), MrpError> {
        if self.pos + size > self.data.len() {
            return Err(MrpError::BadOffset {
                field,
                value: self.pos as u32,
                size: self.data.len(),
            });
        }
        Ok(())
    }

    fn u32(&mut self, field: &'static str) -> Result<u32, MrpError> {
        self.need(4, field)?;
        let raw: [u8; 4] = self.data[self.pos..self.pos + 4]
            .try_into()
            .expect("length checked");
        self.pos += 4;
        Ok(u32::from_le_bytes(raw))
    }

    fn byte(&mut self, field: &'static str) -> Result<u8, MrpError> {
        self.need(1, field)?;
        let value = self.data[self.pos];
        self.pos += 1;
        Ok(value)
    }

    /// Reads a length-prefixed byte string; the stored length includes the
    /// trailing NUL the writer appends, which is dropped here.
    fn bytes(&mut self, field: &'static str) -> Result<Vec<u8>, MrpError> {
        let size = self.u32(field)? as usize;
        if size == 0 {
            return Ok(Vec::new());
        }
        self.need(size, field)?;
        let raw = self.data[self.pos..self.pos + size].to_vec();
        self.pos += size;
        // The chunk writer appends exactly one trailing NUL, which the
        // runtime's string loader drops; mirror that instead of trimming all
        // zero bytes, so strings ending in 0x00 stay faithful.
        Ok(raw[..raw.len() - 1].to_vec())
    }

    fn read_proto(&mut self) -> Result<Proto, MrpError> {
        self.depth += 1;
        if self.depth > MAX_PROTO_DEPTH {
            return Err(MrpError::Unsupported(format!(
                "chunk nesting deeper than {MAX_PROTO_DEPTH}"
            )));
        }

        let source = self.bytes("proto source")?;
        let line_defined = self.u32("line_defined")?;
        let num_upvalues = self.byte("num_upvalues")?;
        let num_params = self.byte("num_params")?;
        let is_vararg = self.byte("is_vararg")?;
        let max_stack_size = self.byte("max_stack_size")?;

        let line_count = self.u32("line_info size")?;
        let mut line_info = Vec::with_capacity(line_count.min(1 << 16) as usize);
        for _ in 0..line_count {
            line_info.push(self.u32("line_info entry")?);
        }

        let local_count = self.u32("local count")?;
        let mut locals = Vec::with_capacity(local_count.min(1 << 16) as usize);
        for _ in 0..local_count {
            let name = self.bytes("local name")?;
            let start_pc = self.u32("local start_pc")?;
            let end_pc = self.u32("local end_pc")?;
            locals.push(LocalVar {
                name,
                start_pc,
                end_pc,
            });
        }

        let upvalue_count = self.u32("upvalue count")?;
        let mut upvalues = Vec::with_capacity(upvalue_count.min(1 << 16) as usize);
        for _ in 0..upvalue_count {
            upvalues.push(self.bytes("upvalue name")?);
        }

        let constant_count = self.u32("constant count")?;
        let mut constants = Vec::with_capacity(constant_count.min(1 << 16) as usize);
        for _ in 0..constant_count {
            let tag = self.byte("constant tag")?;
            let constant = match tag {
                0 => Constant::Nil,
                3 => {
                    let raw = self.u32("constant number")?;
                    Constant::Number(raw as i32)
                }
                4 => Constant::Str(self.bytes("constant string")?),
                other => {
                    return Err(MrpError::Unsupported(format!(
                        "unknown constant type {other} at offset {:#x}",
                        self.pos - 1
                    )))
                }
            };
            constants.push(constant);
        }

        let proto_count = self.u32("nested proto count")?;
        let mut protos = Vec::with_capacity(proto_count.min(1 << 16) as usize);
        for _ in 0..proto_count {
            protos.push(self.read_proto()?);
        }

        let code_count = self.u32("code size")?;
        let mut code = Vec::with_capacity(code_count.min(1 << 20) as usize);
        for _ in 0..code_count {
            let raw = self.u32("instruction")?;
            code.push(Instruction(raw));
        }

        self.depth -= 1;
        Ok(Proto {
            source: if source.is_empty() {
                None
            } else {
                Some(source)
            },
            line_defined,
            num_upvalues,
            num_params,
            is_vararg,
            max_stack_size,
            line_info,
            locals,
            upvalues,
            constants,
            protos,
            code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes a chunk back to bytes using the same layout, so tests can
    /// build synthetic chunks without hand-assembling byte arrays.
    fn write_chunk(proto: &Proto) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&CHUNK_MAGIC);
        out.push(0x80);
        out.push(1);
        write_proto(proto, &mut out);
        out
    }

    fn write_str(value: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(&((value.len() + 1) as u32).to_le_bytes());
        out.extend_from_slice(value);
        out.push(0);
    }

    fn write_proto(proto: &Proto, out: &mut Vec<u8>) {
        match &proto.source {
            Some(source) => write_str(source, out),
            None => out.extend_from_slice(&0u32.to_le_bytes()),
        }
        out.extend_from_slice(&proto.line_defined.to_le_bytes());
        out.push(proto.num_upvalues);
        out.push(proto.num_params);
        out.push(proto.is_vararg);
        out.push(proto.max_stack_size);
        out.extend_from_slice(&(proto.line_info.len() as u32).to_le_bytes());
        for line in &proto.line_info {
            out.extend_from_slice(&line.to_le_bytes());
        }
        out.extend_from_slice(&(proto.locals.len() as u32).to_le_bytes());
        for local in &proto.locals {
            write_str(&local.name, out);
            out.extend_from_slice(&local.start_pc.to_le_bytes());
            out.extend_from_slice(&local.end_pc.to_le_bytes());
        }
        out.extend_from_slice(&(proto.upvalues.len() as u32).to_le_bytes());
        for upvalue in &proto.upvalues {
            write_str(upvalue, out);
        }
        out.extend_from_slice(&(proto.constants.len() as u32).to_le_bytes());
        for constant in &proto.constants {
            match constant {
                Constant::Nil => out.push(0),
                Constant::Number(value) => {
                    out.push(3);
                    out.extend_from_slice(&(*value).to_le_bytes());
                }
                Constant::Str(value) => {
                    out.push(4);
                    write_str(value, out);
                }
            }
        }
        out.extend_from_slice(&(proto.protos.len() as u32).to_le_bytes());
        for nested in &proto.protos {
            write_proto(nested, out);
        }
        out.extend_from_slice(&(proto.code.len() as u32).to_le_bytes());
        for inst in &proto.code {
            out.extend_from_slice(&inst.0.to_le_bytes());
        }
    }

    fn sample_proto() -> Proto {
        Proto {
            source: Some(b"@start.mr".to_vec()),
            line_defined: 0,
            num_upvalues: 0,
            num_params: 0,
            is_vararg: 0,
            max_stack_size: 13,
            line_info: vec![2, 2, 2, 52],
            locals: vec![LocalVar {
                name: b"x".to_vec(),
                start_pc: 1,
                end_pc: 3,
            }],
            upvalues: vec![],
            constants: vec![
                Constant::Nil,
                Constant::Number(1500),
                Constant::Str(b"mrc_clearScreen".to_vec()),
            ],
            protos: vec![Proto {
                source: None,
                line_defined: 5,
                num_upvalues: 0,
                num_params: 1,
                is_vararg: 0,
                max_stack_size: 2,
                line_info: vec![],
                locals: vec![],
                upvalues: vec![],
                constants: vec![],
                protos: vec![],
                code: vec![Instruction(27)], // bare RETURN
            }],
            code: vec![
                Instruction(0),  // MOVE A=0 B=0 C=0
                Instruction(27), // RETURN
            ],
        }
    }

    #[test]
    fn roundtrips_synthetic_chunk() {
        let bytes = write_chunk(&sample_proto());
        let chunk = Chunk::parse(&bytes).expect("chunk must parse");

        assert_eq!(chunk.proto.source.as_deref(), Some(b"@start.mr".as_slice()));
        assert_eq!(chunk.proto.max_stack_size, 13);
        assert_eq!(chunk.proto.line_info, vec![2, 2, 2, 52]);
        assert_eq!(chunk.proto.locals.len(), 1);
        assert_eq!(chunk.proto.locals[0].name, b"x");
        assert_eq!(chunk.proto.constants[1], Constant::Number(1500));
        assert_eq!(
            chunk.proto.constants[2],
            Constant::Str(b"mrc_clearScreen".to_vec())
        );
        assert_eq!(chunk.proto.protos.len(), 1);
        assert!(chunk.proto.code_is_plausible());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = write_chunk(&sample_proto());
        bytes[0] = b'X';

        assert!(matches!(
            Chunk::parse(&bytes),
            Err(MrpError::BadMagic { .. })
        ));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = write_chunk(&sample_proto());
        bytes.push(0);

        assert!(matches!(
            Chunk::parse(&bytes),
            Err(MrpError::Unsupported(_))
        ));
    }

    /// Real-corpus test: hello.mrp's `start.mr` must parse byte-exactly and
    /// expose the SDK boilerplate globals. Requires the local reference
    /// clone, so it stays ignored like the other asset-backed tests.
    #[test]
    #[ignore = "requires the local reference corpus under tmp/"]
    fn parses_hello_mrp_start_script() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tmp/mrpemu/sdcard/mythroad/hello.mrp"
        );
        let package = crate::package::MrpPackage::from_path(path).expect("hello.mrp must load");
        let script = package.start_script().expect("start.mr must exist");

        let chunk = Chunk::parse(&script).expect("start.mr must be a valid chunk");
        assert!(chunk.proto.code_is_plausible());
        let has_com = chunk
            .proto
            .constants
            .iter()
            .any(|c| matches!(c, Constant::Str(s) if s.as_slice() == b"_com"));
        assert!(has_com, "expected the _com global in hello's constants");
    }
}
