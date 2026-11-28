//! MiniJ instruction encoding (Lua 5.0.2 layout, 35 opcodes).
//!
//! MiniJ bytecode uses the unmodified Lua 5.0.2 instruction set — 35
//! opcodes ending at `OP_CLOSURE` (the set has no `OP_TESTSET`,
//! `OP_FORPREP`, or `OP_VARARG`; `OP_RETURN` is 27). Instructions are
//! little-endian `u32` words laid out from bit 0 upward as
//! `op(6) / C(9) / B(9) / A(8)`, so the destination register sits in the
//! top byte and `Bx` spans the C+B range (bits 6..23). Signed jump offsets
//! subtract [`OFFSET_SBX`] from the unsigned `Bx`.

/// `OP_RETURN` opcode value; every chunk's last instruction is a return.
pub const OP_RETURN: u8 = 27;

pub const SIZE_OP: u32 = 6;
pub const SIZE_A: u32 = 8;
pub const SIZE_B: u32 = 9;
pub const SIZE_C: u32 = 9;
/// The `Bx` field occupies the combined C+B bit range (Lua 5.0 layout).
pub const SIZE_BX: u32 = SIZE_C + SIZE_B;
/// Bias converting unsigned `Bx` to signed `sBx`.
pub const OFFSET_SBX: i32 = (1 << (SIZE_BX - 1)) - 1;

/// Raw 32-bit instruction word with field accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Instruction(pub u32);

impl Instruction {
    pub fn op(self) -> u8 {
        (self.0 & mask(SIZE_OP)) as u8
    }

    /// Destination/first operand, in the top byte (bits 24..31).
    pub fn a(self) -> u32 {
        (self.0 >> (SIZE_OP + SIZE_C + SIZE_B)) & mask(SIZE_A)
    }

    /// Third operand / right-hand RK (bits 6..14).
    pub fn c(self) -> u32 {
        (self.0 >> SIZE_OP) & mask(SIZE_C)
    }

    /// Second operand / left-hand RK (bits 15..23).
    pub fn b(self) -> u32 {
        (self.0 >> (SIZE_OP + SIZE_C)) & mask(SIZE_B)
    }

    /// Unsigned 18-bit `Bx` operand (bits 6..23).
    pub fn bx(self) -> i32 {
        ((self.0 >> SIZE_OP) & mask(SIZE_BX)) as i32
    }

    /// Signed jump offset: `Bx` biased by [`OFFSET_SBX`].
    pub fn sbx(self) -> i32 {
        self.bx() - OFFSET_SBX
    }
}

/// Builds an `iABC` instruction word from an opcode and its three operands.
pub fn encode(op: Op, a: u32, b: u32, c: u32) -> Instruction {
    Instruction(
        (op as u32 & mask(SIZE_OP))
            | ((c & mask(SIZE_C)) << SIZE_OP)
            | ((b & mask(SIZE_B)) << (SIZE_OP + SIZE_C))
            | ((a & mask(SIZE_A)) << (SIZE_OP + SIZE_C + SIZE_B)),
    )
}

/// Builds an `iABx` instruction word from an opcode, destination and operand.
pub fn encode_bx(op: Op, a: u32, bx: u32) -> Instruction {
    Instruction(
        (op as u32 & mask(SIZE_OP))
            | ((bx & mask(SIZE_BX)) << SIZE_OP)
            | ((a & mask(SIZE_A)) << (SIZE_OP + SIZE_C + SIZE_B)),
    )
}

fn mask(bits: u32) -> u32 {
    (1 << bits) - 1
}

/// The 35 MiniJ opcodes (the Lua 5.0.2 set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    Move = 0,
    LoadK = 1,
    LoadBool = 2,
    LoadNil = 3,
    GetUpval = 4,
    GetGlobal = 5,
    GetTable = 6,
    SetGlobal = 7,
    Setupval = 8,
    SetTable = 9,
    NewTable = 10,
    Self_ = 11,
    Add = 12,
    Sub = 13,
    Mul = 14,
    Div = 15,
    Pow = 16,
    Unm = 17,
    Not = 18,
    Concat = 19,
    Jmp = 20,
    Eq = 21,
    Lt = 22,
    Le = 23,
    Test = 24,
    Call = 25,
    Tailcall = 26,
    Return = 27,
    ForLoop = 28,
    TForLoop = 29,
    TForPrep = 30,
    SetList = 31,
    SetListO = 32,
    Close = 33,
    Closure = 34,
}

impl Op {
    /// Maps a raw opcode field; `None` for values outside the 35-opcode set.
    pub fn from_u8(raw: u8) -> Option<Op> {
        if raw > Self::Closure as u8 {
            return None;
        }
        // The variants are declared in discriminant order, so a transmute
        // would be valid; a match keeps it safe against future reordering.
        let op = match raw {
            0 => Op::Move,
            1 => Op::LoadK,
            2 => Op::LoadBool,
            3 => Op::LoadNil,
            4 => Op::GetUpval,
            5 => Op::GetGlobal,
            6 => Op::GetTable,
            7 => Op::SetGlobal,
            8 => Op::Setupval,
            9 => Op::SetTable,
            10 => Op::NewTable,
            11 => Op::Self_,
            12 => Op::Add,
            13 => Op::Sub,
            14 => Op::Mul,
            15 => Op::Div,
            16 => Op::Pow,
            17 => Op::Unm,
            18 => Op::Not,
            19 => Op::Concat,
            20 => Op::Jmp,
            21 => Op::Eq,
            22 => Op::Lt,
            23 => Op::Le,
            24 => Op::Test,
            25 => Op::Call,
            26 => Op::Tailcall,
            27 => Op::Return,
            28 => Op::ForLoop,
            29 => Op::TForLoop,
            30 => Op::TForPrep,
            31 => Op::SetList,
            32 => Op::SetListO,
            33 => Op::Close,
            34 => Op::Closure,
            _ => return None,
        };
        Some(op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_iabc_fields() {
        let inst = encode(Op::Call, 3, 2, 1);
        assert_eq!(inst.op(), Op::Call as u8);
        assert_eq!(inst.a(), 3);
        assert_eq!(inst.b(), 2);
        assert_eq!(inst.c(), 1);
        // Round-trip through the raw-word interpretation.
        let raw = Instruction(inst.0);
        assert_eq!((raw.op(), raw.a(), raw.b(), raw.c()), (25, 3, 2, 1));
    }

    #[test]
    fn decodes_bx_and_sbx() {
        assert_eq!(encode_bx(Op::LoadK, 5, 300).bx(), 300);
        assert_eq!(encode_bx(Op::LoadK, 5, 300).a(), 5);
        // The signed offset is `Bx - 131071`: zero sits at the bias.
        assert_eq!(Instruction((131_071 << 6) | Op::Jmp as u32).sbx(), 0);
        assert_eq!(Instruction((131_070 << 6) | Op::Jmp as u32).sbx(), -1);
        assert_eq!(Instruction((131_073 << 6) | Op::Jmp as u32).sbx(), 2);
        // The smallest encodable offset is -(2^17 - 1).
        assert_eq!(Instruction(Op::Jmp as u32).sbx(), -131_071);
    }

    /// Regression: instructions from a real application's `start.mr` must
    /// decode with the Lua 5.0 field positions — the callee/global index
    /// every later stage relies on lives in bits 6..23, the destination in
    /// the top byte. Each word below previously decoded to a wrong operand
    /// under the shifted (A-in-low-byte) reading.
    #[test]
    fn decodes_reference_startup_words() {
        // `_com(3629, 2913)`: Call A=0 B=3 C=1 (callee R0, two args).
        let call = Instruction(0x0001_8059);
        assert_eq!(call.op(), Op::Call as u8);
        assert_eq!((call.a(), call.b(), call.c()), (0, 3, 1));

        // `LoadK R1 = K1` where K1 = 3629.
        let load = Instruction(0x0100_0041);
        assert_eq!((load.op(), load.a(), load.bx()), (Op::LoadK as u8, 1, 1));

        // `_mr_c_load = function(...) end`: SetGlobal A=0 Bx=3 (K3).
        let setg = Instruction(0x0000_00c7);
        assert_eq!(
            (setg.op(), setg.a(), setg.bx()),
            (Op::SetGlobal as u8, 0, 3)
        );

        // The second of five `Closure` definitions: Bx selects nested
        // proto 1 while A reuses register 0.
        let closure = Instruction(0x0000_0062);
        assert_eq!(
            (closure.op(), closure.a(), closure.bx()),
            (Op::Closure as u8, 0, 1)
        );

        // RK operand: values >= 250 address constants (255 -> K5 == 0).
        let add = Instruction(0x0402_3fcc);
        assert_eq!((add.op(), add.a(), add.b()), (Op::Add as u8, 4, 4));
        assert_eq!(add.c(), 255);
    }

    #[test]
    fn opcode_table_matches_discriminants() {
        for raw in 0..=34u8 {
            let op = Op::from_u8(raw).expect("0..=34 are valid opcodes");
            assert_eq!(op as u8, raw);
        }
        assert!(Op::from_u8(35).is_none());
        assert_eq!(Op::from_u8(OP_RETURN), Some(Op::Return));
    }
}
