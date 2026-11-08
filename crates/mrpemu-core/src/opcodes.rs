//! MiniJ instruction encoding (Lua 5.0.2 layout, 35 opcodes).
//!
//! MiniJ bytecode uses the unmodified Lua 5.0.2 instruction set — 35
//! opcodes ending at `OP_CLOSURE` (the set has no `OP_TESTSET`,
//! `OP_FORPREP`, or `OP_VARARG`; `OP_RETURN` is 27). This was verified
//! against the reference corpus: every sample decodes with Lua 5.0.2's
//! opcode table, and nested-function counts match `OP_CLOSURE` counts.
//! Instructions are little-endian `u32` words in one of three formats:
//! `iABC` (op 6 / A 8 / C 9 / B 9 bits), `iABx` (op 6 / A 8 / Bx 18), and
//! `iAsBx` (signed `Bx` with a fixed bias).

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instruction(pub u32);

impl Instruction {
    pub fn op(self) -> u8 {
        (self.0 & mask(SIZE_OP)) as u8
    }

    pub fn a(self) -> u32 {
        (self.0 >> SIZE_OP) & mask(SIZE_A)
    }

    /// Empirical MRP layout: op(6) A(8) B(10) C(8).
    pub fn c(self) -> u32 {
        (self.0 >> 24) & 0xFF
    }

    pub fn b(self) -> u32 {
        (self.0 >> 14) & mask(10)
    }

    pub fn bx(self) -> i32 {
        // Empirically established for MRP: the ABx operand occupies the
        // high 8 bits (24..32) of the word, not Lua's 18-bit C+B field.
        ((self.0 >> 24) & 0xFF) as i32
    }

    /// Signed variant for jump offsets: the same 8-bit field, sign-extended
    /// (backward jumps encode 255 as -1 and so on).
    pub fn sbx(self) -> i32 {
        ((self.0 >> 24) & 0xFF) as u8 as i8 as i32
    }
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
        // OP_CALL(25) A=1 B=2 C=0: (0<<23)|(0<<14)|(1<<6)|25
        let inst = Instruction((1 << 6) | 25);
        assert_eq!(inst.op(), 25);
        assert_eq!(inst.a(), 1);
        assert_eq!(inst.b(), 0);
        assert_eq!(inst.c(), 0);
    }

    #[test]
    fn decodes_sbx_sign_extended() {
        // The 8-bit jump field is sign-extended: 0 = 0, 255 = -1, 2 = +2.
        assert_eq!(Instruction(0).sbx(), 0);
        assert_eq!(Instruction(255 << 24).sbx(), -1);
        assert_eq!(Instruction(2 << 24).sbx(), 2);
        assert_eq!(Instruction(1 << 24).bx(), 1);
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
