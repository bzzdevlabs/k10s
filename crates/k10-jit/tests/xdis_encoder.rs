//! Differential fuzzing of the x86-64 encoder (`k10_jit::x64`) against objdump: each random instruction must decode
//! to exactly the expected length and Intel text (all GPRs incl. spl/bpl/sil/dil and r8-r15, xmm0-15, rsp/r12 SIB
//! and rbp/r13 disp8 cases, disp8/disp32 and imm sign-extension boundaries, jump and RIP-relative targets).
//! `K10_FUZZ_SEED`, `K10_FUZZ_ITERS` (default 30 000); `K10_FUZZ_LONG=1`: 2 000 000.

mod xdis;

use k10_jit::x64::*;
use Reg::*;

/// Instruction-form choices on top of the generator's RNG.
struct Rng(k10_ir::gen::Rng);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0.next_u64()
    }
    fn below(&mut self, n: u64) -> u64 {
        self.0.below(n)
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        self.0.pick(xs)
    }
    fn bool(&mut self) -> bool {
        self.next() & 1 == 1
    }
    fn reg(&mut self) -> Reg {
        Reg::from_id(self.below(16) as u8)
    }
    fn xmm(&mut self) -> Xmm {
        Xmm(self.below(16) as u8)
    }
    fn cond(&mut self) -> Cond {
        CONDS[self.below(16) as usize]
    }
    /// Interesting displacements: 0, the disp8/disp32 boundaries, random small and large values.
    fn disp(&mut self) -> i32 {
        match self.below(12) {
            0..=2 => 0,
            3 => self.pick(&[127, -128, 128, -129, 1, -1, 8, -8]),
            4 => i32::MIN,
            5 => i32::MAX,
            6..=8 => (self.next() as i8) as i32,
            _ => self.next() as i32,
        }
    }
    fn imm32(&mut self) -> i32 {
        match self.below(8) {
            0 => 0,
            1 => self.pick(&[127, -128, 128, -129, 1, -1]),
            2 => i32::MIN,
            3 => i32::MAX,
            4 | 5 => (self.next() as i8) as i32,
            _ => self.next() as i32,
        }
    }
    fn imm64(&mut self) -> i64 {
        match self.below(6) {
            0 => self.imm32() as i64,
            1 => self.imm32() as u32 as i64,
            2 => self.pick(&[u32::MAX as i64, u32::MAX as i64 + 1, i64::MIN, i64::MAX, -1, i32::MIN as i64 - 1]),
            _ => self.next() as i64,
        }
    }
    /// A memory operand; `rip` allows RIP-relative ones (to one of `labels`).
    fn mem(&mut self, rip: bool, labels: &[Label]) -> Mem {
        if rip && self.below(8) == 0 {
            return Mem::rip(self.pick(labels));
        }
        let base = self.reg();
        let index = if self.below(3) == 0 {
            let mut i = self.reg();
            while i == Rsp {
                i = self.reg();
            }
            Some((i, self.below(4) as u8))
        } else {
            None
        };
        Mem { base: Base::Reg(base), index, disp: self.disp() }
    }
}

const CONDS: [Cond; 16] = [
    Cond::O,
    Cond::No,
    Cond::B,
    Cond::Ae,
    Cond::E,
    Cond::Ne,
    Cond::Be,
    Cond::A,
    Cond::S,
    Cond::Ns,
    Cond::P,
    Cond::Np,
    Cond::L,
    Cond::Ge,
    Cond::Le,
    Cond::G,
];

// ---------------------------------------------------------------------------------------- expected objdump text

fn r64(r: Reg) -> &'static str {
    ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"]
        [r.id() as usize]
}
fn r32(r: Reg) -> &'static str {
    [
        "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d", "r13d", "r14d",
        "r15d",
    ][r.id() as usize]
}
fn r16(r: Reg) -> &'static str {
    ["ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w", "r14w", "r15w"]
        [r.id() as usize]
}
/// 8-bit names with REX (without it spl/bpl/sil/dil would decode as ah/ch/dh/bh).
fn r8(r: Reg) -> &'static str {
    ["al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b", "r13b", "r14b", "r15b"]
        [r.id() as usize]
}
fn xmm(x: Xmm) -> String {
    format!("xmm{}", x.0)
}
fn cc(c: Cond) -> &'static str {
    ["o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g"][c as usize]
}
fn hex(v: u64) -> String {
    format!("0x{v:x}")
}
fn alu_name(op: Alu) -> &'static str {
    match op {
        Alu::Add => "add",
        Alu::Or => "or",
        Alu::Adc => "adc",
        Alu::Sbb => "sbb",
        Alu::And => "and",
        Alu::Sub => "sub",
        Alu::Xor => "xor",
        Alu::Cmp => "cmp",
    }
}

/// `[base+index*scale±disp]` as objdump prints it; rbp/r13 bases always carry a disp8 (mod=00 would mean RIP/disp32).
fn mem_str(m: Mem) -> String {
    let Base::Reg(b) = m.base else { return "[rip+?]".into() };
    let mut s = format!("[{}", r64(b));
    if let Some((i, sc)) = m.index {
        s += &format!("+{}*{}", r64(i), 1 << sc);
    }
    if m.disp != 0 || b.id() & 7 == 5 {
        if m.disp < 0 {
            s += &format!("-0x{:x}", (m.disp as i64).unsigned_abs());
        } else {
            s += &format!("+0x{:x}", m.disp);
        }
    }
    s + "]"
}

fn sized(size: &str, m: Mem) -> String {
    format!("{size} PTR {}", mem_str(m))
}

fn rm64(rm: RM) -> String {
    match rm {
        RM::R(r) => r64(r).into(),
        RM::M(m) => sized("QWORD", m),
    }
}

/// What one emitted instruction must disassemble to.
struct Expect {
    /// Rust call, for failure messages.
    call: String,
    text: String,
    /// Direct jump/call: the target label (its address replaces `{T}` in `text`).
    jump: Option<Label>,
    /// RIP-relative operand: the label objdump's `# 0x…` comment must point to.
    rip: Option<Label>,
}

fn rip_of(rm: RM) -> Option<Label> {
    match rm {
        RM::M(Mem { base: Base::Rip(l), .. }) => Some(l),
        _ => None,
    }
}

/// Emits one random instruction and returns its expectation.
fn gen(a: &mut Asm, g: &mut Rng, labels: &[Label]) -> Expect {
    let x = |call: String, text: String| Expect { call, text, jump: None, rip: None };
    let w = g.below(60);
    match w {
        0 => {
            let (d, s) = (g.reg(), g.reg());
            a.mov_rr(d, s);
            x(format!("mov_rr({d:?},{s:?})"), format!("mov {},{}", r64(d), r64(s)))
        }
        1 => {
            let (d, s) = (g.reg(), g.reg());
            a.mov_rr32(d, s);
            x(format!("mov_rr32({d:?},{s:?})"), format!("mov {},{}", r32(d), r32(s)))
        }
        2 | 3 => {
            let (sz, d, m) = (g.pick(&[Size::S8, Size::S16, Size::S32, Size::S64]), g.reg(), g.mem(true, labels));
            a.load(sz, d, m);
            let t = match sz {
                Size::S64 => format!("mov {},{}", r64(d), sized("QWORD", m)),
                Size::S32 => format!("mov {},{}", r32(d), sized("DWORD", m)),
                Size::S16 => format!("movzx {},{}", r32(d), sized("WORD", m)),
                Size::S8 => format!("movzx {},{}", r32(d), sized("BYTE", m)),
            };
            Expect { rip: rip_of(RM::M(m)), ..x(format!("load({sz:?},{d:?},{m:?})"), t) }
        }
        4 | 5 => {
            let (sz, m, s) = (g.pick(&[Size::S8, Size::S16, Size::S32, Size::S64]), g.mem(true, labels), g.reg());
            a.store(sz, m, s);
            let t = match sz {
                Size::S64 => format!("mov {},{}", sized("QWORD", m), r64(s)),
                Size::S32 => format!("mov {},{}", sized("DWORD", m), r32(s)),
                Size::S16 => format!("mov {},{}", sized("WORD", m), r16(s)),
                Size::S8 => format!("mov {},{}", sized("BYTE", m), r8(s)),
            };
            Expect { rip: rip_of(RM::M(m)), ..x(format!("store({sz:?},{m:?},{s:?})"), t) }
        }
        6 => {
            let (sz, m, imm) = (g.pick(&[Size::S8, Size::S16, Size::S32, Size::S64]), g.mem(false, labels), g.imm32());
            a.store_imm(sz, m, imm);
            let t = match sz {
                Size::S64 => format!("mov {},{}", sized("QWORD", m), hex(imm as i64 as u64)),
                Size::S32 => format!("mov {},{}", sized("DWORD", m), hex(imm as u32 as u64)),
                Size::S16 => format!("mov {},{}", sized("WORD", m), hex(imm as u16 as u64)),
                Size::S8 => format!("mov {},{}", sized("BYTE", m), hex(imm as u8 as u64)),
            };
            x(format!("store_imm({sz:?},{m:?},{imm})"), t)
        }
        7 | 8 => {
            let (d, imm) = (g.reg(), g.imm64());
            a.mov_ri(d, imm);
            let t = if imm as u64 <= u32::MAX as u64 {
                format!("mov {},{}", r32(d), hex(imm as u64))
            } else if imm as i32 as i64 == imm {
                format!("mov {},{}", r64(d), hex(imm as u64))
            } else {
                format!("movabs {},{}", r64(d), hex(imm as u64))
            };
            x(format!("mov_ri({d:?},{imm})"), t)
        }
        9 => {
            let d = g.reg();
            a.zero(d);
            x(format!("zero({d:?})"), format!("xor {},{}", r32(d), r32(d)))
        }
        10..=14 => {
            let d = g.reg();
            let src: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) };
            let (call, t) = match w {
                10 => {
                    a.movzx8(d, src);
                    let s = match src {
                        RM::R(r) => r8(r).to_string(),
                        RM::M(m) => sized("BYTE", m),
                    };
                    ("movzx8", format!("movzx {},{s}", r32(d)))
                }
                11 => {
                    a.movzx16(d, src);
                    let s = match src {
                        RM::R(r) => r16(r).to_string(),
                        RM::M(m) => sized("WORD", m),
                    };
                    ("movzx16", format!("movzx {},{s}", r32(d)))
                }
                12 => {
                    a.movsx8(d, src);
                    let s = match src {
                        RM::R(r) => r8(r).to_string(),
                        RM::M(m) => sized("BYTE", m),
                    };
                    ("movsx8", format!("movsx {},{s}", r64(d)))
                }
                13 => {
                    a.movsx16(d, src);
                    let s = match src {
                        RM::R(r) => r16(r).to_string(),
                        RM::M(m) => sized("WORD", m),
                    };
                    ("movsx16", format!("movsx {},{s}", r64(d)))
                }
                _ => {
                    if g.bool() {
                        a.movsxd(d, src);
                        let s = match src {
                            RM::R(r) => r32(r).to_string(),
                            RM::M(m) => sized("DWORD", m),
                        };
                        ("movsxd", format!("movsxd {},{s}", r64(d)))
                    } else {
                        a.mov32(d, src);
                        let s = match src {
                            RM::R(r) => r32(r).to_string(),
                            RM::M(m) => sized("DWORD", m),
                        };
                        ("mov32", format!("mov {},{s}", r32(d)))
                    }
                }
            };
            Expect { rip: rip_of(src), ..x(format!("{call}({d:?},{src:?})"), t) }
        }
        15 => {
            let (d, m) = (g.reg(), g.mem(true, labels));
            a.lea(d, m);
            Expect { rip: rip_of(RM::M(m)), ..x(format!("lea({d:?},{m:?})"), format!("lea {},{}", r64(d), mem_str(m))) }
        }
        16..=19 => {
            let (op, d) =
                (g.pick(&[Alu::Add, Alu::Or, Alu::Adc, Alu::Sbb, Alu::And, Alu::Sub, Alu::Xor, Alu::Cmp]), g.reg());
            let src: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) };
            a.alu(op, d, src);
            Expect {
                rip: rip_of(src),
                ..x(format!("alu({op:?},{d:?},{src:?})"), format!("{} {},{}", alu_name(op), r64(d), rm64(src)))
            }
        }
        20 => {
            let (op, m, s) = (
                g.pick(&[Alu::Add, Alu::Or, Alu::Adc, Alu::Sbb, Alu::And, Alu::Sub, Alu::Xor, Alu::Cmp]),
                g.mem(true, labels),
                g.reg(),
            );
            a.alu_mr(op, m, s);
            Expect {
                rip: rip_of(RM::M(m)),
                ..x(format!("alu_mr({op:?},{m:?},{s:?})"), format!("{} {},{}", alu_name(op), sized("QWORD", m), r64(s)))
            }
        }
        21..=23 => {
            let op = g.pick(&[Alu::Add, Alu::Or, Alu::Adc, Alu::Sbb, Alu::And, Alu::Sub, Alu::Xor, Alu::Cmp]);
            let dst: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(false, labels)) };
            let imm = g.imm32();
            if g.bool() {
                a.alu_ri(op, dst, imm);
                x(
                    format!("alu_ri({op:?},{dst:?},{imm})"),
                    format!("{} {},{}", alu_name(op), rm64(dst), hex(imm as i64 as u64)),
                )
            } else {
                a.alu_ri32(op, dst, imm);
                let d = match dst {
                    RM::R(r) => r32(r).to_string(),
                    RM::M(m) => sized("DWORD", m),
                };
                x(format!("alu_ri32({op:?},{dst:?},{imm})"), format!("{} {d},{}", alu_name(op), hex(imm as u32 as u64)))
            }
        }
        24 => {
            let (am, b) = (if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) }, g.reg());
            a.test_rr(am, b);
            Expect { rip: rip_of(am), ..x(format!("test_rr({am:?},{b:?})"), format!("test {},{}", rm64(am), r64(b))) }
        }
        25 => {
            let (p, q) = (g.reg(), g.reg());
            a.test32(p, q);
            x(format!("test32({p:?},{q:?})"), format!("test {},{}", r32(p), r32(q)))
        }
        26 => {
            let d = g.reg();
            let src: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) };
            a.imul(d, src);
            Expect { rip: rip_of(src), ..x(format!("imul({d:?},{src:?})"), format!("imul {},{}", r64(d), rm64(src))) }
        }
        27 => {
            let d = g.reg();
            let src: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(false, labels)) };
            let imm = g.imm32();
            a.imul_i(d, src, imm);
            x(
                format!("imul_i({d:?},{src:?},{imm})"),
                format!("imul {},{},{}", r64(d), rm64(src), hex(imm as i64 as u64)),
            )
        }
        28 | 29 => {
            let op = g.pick(&[Unary::Not, Unary::Neg, Unary::Mul, Unary::Imul, Unary::Div, Unary::Idiv]);
            let rm: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) };
            a.unary(op, rm);
            let n = match op {
                Unary::Not => "not",
                Unary::Neg => "neg",
                Unary::Mul => "mul",
                Unary::Imul => "imul",
                Unary::Div => "div",
                Unary::Idiv => "idiv",
            };
            Expect { rip: rip_of(rm), ..x(format!("unary({op:?},{rm:?})"), format!("{n} {}", rm64(rm))) }
        }
        30 => {
            a.cqo();
            x("cqo()".into(), "cqo".into())
        }
        31 | 32 => {
            let op = g.pick(&[Shift::Rol, Shift::Ror, Shift::Shl, Shift::Shr, Shift::Sar]);
            let n = match op {
                Shift::Rol => "rol",
                Shift::Ror => "ror",
                Shift::Shl => "shl",
                Shift::Shr => "shr",
                Shift::Sar => "sar",
            };
            if g.bool() {
                let rm: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) };
                a.shift_cl(op, rm);
                Expect { rip: rip_of(rm), ..x(format!("shift_cl({op:?},{rm:?})"), format!("{n} {},cl", rm64(rm))) }
            } else {
                let rm: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(false, labels)) };
                let imm = g.below(256) as u8;
                a.shift_ri(op, rm, imm);
                x(format!("shift_ri({op:?},{rm:?},{imm})"), format!("{n} {},{}", rm64(rm), hex(imm as u64)))
            }
        }
        33 => {
            let (c, d) = (g.cond(), g.reg());
            a.setcc(c, d);
            x(format!("setcc({c:?},{d:?})"), format!("set{} {}", cc(c), r8(d)))
        }
        34 => {
            let (c, d) = (g.cond(), g.reg());
            let src: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) };
            a.cmov(c, d, src);
            Expect {
                rip: rip_of(src),
                ..x(format!("cmov({c:?},{d:?},{src:?})"), format!("cmov{} {},{}", cc(c), r64(d), rm64(src)))
            }
        }
        35 => {
            let d = g.reg();
            let src: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) };
            let k = g.below(5);
            let n = ["bsr", "bsf", "lzcnt", "tzcnt", "popcnt"][k as usize];
            match k {
                0 => a.bsr(d, src),
                1 => a.bsf(d, src),
                2 => a.lzcnt(d, src),
                3 => a.tzcnt(d, src),
                _ => a.popcnt(d, src),
            }
            Expect { rip: rip_of(src), ..x(format!("{n}({d:?},{src:?})"), format!("{n} {},{}", r64(d), rm64(src))) }
        }
        36 => {
            let op = g.pick(&[BitOp::Bt, BitOp::Bts, BitOp::Btr, BitOp::Btc]);
            let rm: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(false, labels)) };
            let bit = g.below(64) as u8;
            a.bit_i(op, rm, bit);
            let n = match op {
                BitOp::Bt => "bt",
                BitOp::Bts => "bts",
                BitOp::Btr => "btr",
                BitOp::Btc => "btc",
            };
            x(format!("bit_i({op:?},{rm:?},{bit})"), format!("{n} {},{}", rm64(rm), hex(bit as u64)))
        }
        37 => {
            let r = g.reg();
            if g.bool() {
                a.push(r);
                x(format!("push({r:?})"), format!("push {}", r64(r)))
            } else {
                a.pop(r);
                x(format!("pop({r:?})"), format!("pop {}", r64(r)))
            }
        }
        38 => {
            let m = g.mem(true, labels);
            a.push_m(m);
            Expect { rip: rip_of(RM::M(m)), ..x(format!("push_m({m:?})"), format!("push {}", sized("QWORD", m))) }
        }
        39 => match g.below(6) {
            0 => {
                a.ret();
                x("ret()".into(), "ret".into())
            }
            1 => {
                a.int3();
                x("int3()".into(), "int3".into())
            }
            2 => {
                a.ud2();
                x("ud2()".into(), "ud2".into())
            }
            3 => {
                a.lfence();
                x("lfence()".into(), "lfence".into())
            }
            4 => {
                a.pause();
                x("pause()".into(), "pause".into())
            }
            _ => {
                a.rep_stosq();
                x("rep_stosq()".into(), "rep stos QWORD PTR es:[rdi],rax".into())
            }
        },
        40..=43 => {
            let l = g.pick(labels);
            let (call, t) = match g.below(3) {
                0 => {
                    a.jmp(l);
                    ("jmp", "jmp {T}".to_string())
                }
                1 => {
                    let c = g.cond();
                    a.jcc(c, l);
                    ("jcc", format!("j{} {{T}}", cc(c)))
                }
                _ => {
                    a.call(l);
                    ("call", "call {T}".to_string())
                }
            };
            Expect { jump: Some(l), ..x(format!("{call}({l:?})"), t) }
        }
        44 => {
            let rm: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) };
            let (n, t) = if g.bool() {
                a.call_rm(rm);
                ("call_rm", "call")
            } else {
                a.jmp_rm(rm);
                ("jmp_rm", "jmp")
            };
            Expect { rip: rip_of(rm), ..x(format!("{n}({rm:?})"), format!("{t} {}", rm64(rm))) }
        }
        45 | 46 => {
            let v = g.xmm();
            let rm: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) };
            if w == 45 {
                a.movq_to_xmm(v, rm);
                Expect {
                    rip: rip_of(rm),
                    ..x(format!("movq_to_xmm({v:?},{rm:?})"), format!("movq {},{}", xmm(v), rm64(rm)))
                }
            } else {
                a.movq_from_xmm(rm, v);
                Expect {
                    rip: rip_of(rm),
                    ..x(format!("movq_from_xmm({rm:?},{v:?})"), format!("movq {},{}", rm64(rm), xmm(v)))
                }
            }
        }
        47..=49 => {
            let op = g.pick(&[Sd::Sqrt, Sd::Add, Sd::Mul, Sd::Sub, Sd::Div]);
            let n = match op {
                Sd::Sqrt => "sqrtsd",
                Sd::Add => "addsd",
                Sd::Mul => "mulsd",
                Sd::Sub => "subsd",
                Sd::Div => "divsd",
            };
            let d = g.xmm();
            if g.bool() {
                let s = g.xmm();
                a.sd(op, d, s);
                x(format!("sd({op:?},{d:?},{s:?})"), format!("{n} {},{}", xmm(d), xmm(s)))
            } else {
                let m = g.mem(true, labels);
                a.sd_m(op, d, m);
                Expect {
                    rip: rip_of(RM::M(m)),
                    ..x(format!("sd_m({op:?},{d:?},{m:?})"), format!("{n} {},{}", xmm(d), sized("QWORD", m)))
                }
            }
        }
        50 => {
            let (d, s, p) = (g.xmm(), g.xmm(), g.below(8) as u8);
            a.cmpsd(d, s, p);
            let n = ["eq", "lt", "le", "unord", "neq", "nlt", "nle", "ord"][p as usize];
            x(format!("cmpsd({d:?},{s:?},{p})"), format!("cmp{n}sd {},{}", xmm(d), xmm(s)))
        }
        51 => {
            let (p, q) = (g.xmm(), g.xmm());
            a.ucomisd(p, q);
            x(format!("ucomisd({p:?},{q:?})"), format!("ucomisd {},{}", xmm(p), xmm(q)))
        }
        52 => {
            let d = g.xmm();
            let rm: RM = if g.bool() { RM::R(g.reg()) } else { RM::M(g.mem(true, labels)) };
            a.cvtsi2sd(d, rm);
            Expect {
                rip: rip_of(rm),
                ..x(format!("cvtsi2sd({d:?},{rm:?})"), format!("cvtsi2sd {},{}", xmm(d), rm64(rm)))
            }
        }
        53 => {
            let (d, s) = (g.reg(), g.xmm());
            a.cvttsd2si(d, s);
            x(format!("cvttsd2si({d:?},{s:?})"), format!("cvttsd2si {},{}", r64(d), xmm(s)))
        }
        54 => {
            let (d, s) = (g.xmm(), g.xmm());
            a.xorpd(d, s);
            x(format!("xorpd({d:?},{s:?})"), format!("xorpd {},{}", xmm(d), xmm(s)))
        }
        _ => {
            // Extra register-register ALU traffic (the codegen's staple).
            let (op, d, s) = (g.pick(&[Alu::Add, Alu::Sub, Alu::And, Alu::Or, Alu::Xor, Alu::Cmp]), g.reg(), g.reg());
            a.alu(op, d, RM::R(s));
            x(format!("alu({op:?},{d:?},{s:?})"), format!("{} {},{}", alu_name(op), r64(d), r64(s)))
        }
    }
}

/// Emits `n` random instructions (labels at random boundaries), disassembles and compares; returns `n`.
fn run_batch(objdump: &std::path::Path, seed: u64, n: usize) -> usize {
    let mut g = Rng(k10_ir::gen::Rng::new(seed));
    let mut a = Asm::new();
    let labels: Vec<Label> = (0..64).map(|_| a.new_label()).collect();
    let mut bound = 0;
    let mut exp: Vec<(u32, u32, Expect)> = Vec::with_capacity(n);
    for _ in 0..n {
        if bound < labels.len() && g.below((n / labels.len()).max(1) as u64) == 0 {
            a.bind(labels[bound]);
            bound += 1;
        }
        let start = a.pos();
        let e = gen(&mut a, &mut g, &labels);
        exp.push((start, a.pos(), e));
    }
    for &l in &labels[bound..] {
        a.bind(l);
    }
    let label_pos: Vec<u32> = labels.iter().map(|&l| a.bound(l).unwrap()).collect();
    let code = a.finish().expect("finish");
    let insns = xdis::disassemble_many(objdump, &[&code]).remove(0);
    let mut k = 0; // index into insns
    for (start, end, e) in &exp {
        let insn =
            insns.get(k).unwrap_or_else(|| panic!("seed {seed:#x}: objdump stopped before {} at {start:#x}", e.call));
        let bytes = &code[*start as usize..*end as usize];
        let ctx = || format!("seed {seed:#x}, {} at {start:#x}, bytes {:02x?}", e.call, bytes);
        assert_eq!(insn.addr, *start as u64, "{}: instruction boundary mismatch (objdump: {:?})", ctx(), insn.text());
        assert_eq!(insn.end(), *end as u64, "{}: length mismatch, objdump decoded {:?}", ctx(), insn.text());
        let mut want = e.text.clone();
        if let Some(l) = e.jump {
            want = want.replace("{T}", &hex(label_pos[l.0 as usize] as u64));
        }
        let mut got = insn.text();
        if let Some(l) = e.rip {
            // `[rip+0x…]` prints the raw displacement; the `# 0x…` comment is the resolved target.
            let target = label_pos[labels.iter().position(|&x| x == l).unwrap()] as u64;
            assert_eq!(
                insn.comment_target,
                Some(target),
                "{}: RIP-relative target (objdump: {:?})",
                ctx(),
                insn.text()
            );
            if let (Some(i), Some(j)) = (got.find("[rip+"), got.find(']')) {
                got.replace_range(i..=j, "[rip+?]");
            }
        }
        assert_eq!(got, want, "{}", ctx());
        k += 1;
    }
    assert_eq!(k, insns.len(), "seed {seed:#x}: objdump decoded extra instructions");
    exp.len()
}

#[test]
fn encoder_matches_objdump() {
    let Some(objdump) = xdis::tool("encoder_matches_objdump") else { return };
    let seed: u64 = std::env::var("K10_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5EED_10C5);
    let long = std::env::var_os("K10_FUZZ_LONG").is_some();
    let total: usize = std::env::var("K10_FUZZ_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(if long {
        2_000_000
    } else {
        30_000
    });
    let batch = 20_000.min(total).max(1);
    let mut done = 0;
    let mut i = 0u64;
    while done < total {
        done += run_batch(&objdump, seed.wrapping_add(i.wrapping_mul(0x1000_0001)), batch.min(total - done));
        i += 1;
    }
    eprintln!("encoder vs objdump: {done} random instructions identical (seed {seed:#x}, {i} batches)");
}

/// Exhaustive bases x indexes x displacements x registers for `mov r64,[m]`, `mov [m],r8` and `lea`.
#[test]
fn addressing_modes_exhaustive() {
    let Some(objdump) = xdis::tool("addressing_modes_exhaustive") else { return };
    let mut a = Asm::new();
    let mut exp = Vec::new();
    let regs = Reg::ALL;
    for &b in &regs {
        for idx in std::iter::once(None).chain(regs.iter().filter(|&&r| r != Rsp).map(|&r| Some(r))) {
            for disp in [0, 1, -128, 127, 128, -129, 0x1234_5678] {
                for (si, &d) in regs.iter().enumerate() {
                    let m = Mem { base: Base::Reg(b), index: idx.map(|i| (i, (si % 4) as u8)), disp };
                    let start = a.pos();
                    match si % 3 {
                        0 => {
                            a.load(Size::S64, d, m);
                            exp.push((start, format!("mov {},{}", r64(d), sized("QWORD", m))));
                        }
                        1 => {
                            a.store(Size::S8, m, d);
                            exp.push((start, format!("mov {},{}", sized("BYTE", m), r8(d))));
                        }
                        _ => {
                            a.lea(d, m);
                            exp.push((start, format!("lea {},{}", r64(d), mem_str(m))));
                        }
                    }
                }
            }
        }
    }
    let code = a.finish().unwrap();
    let insns = xdis::disassemble_many(&objdump, &[&code]).remove(0);
    assert_eq!(insns.len(), exp.len());
    for (insn, (start, want)) in insns.iter().zip(&exp) {
        assert_eq!(insn.addr, *start as u64);
        assert_eq!(&insn.text(), want, "bytes {:02x?}", insn.bytes);
    }
    eprintln!("addressing modes: {} instructions identical", exp.len());
}

/// The harness itself: one flipped REX bit must change what objdump reports.
#[test]
fn harness_detects_a_flipped_rex_bit() {
    let Some(objdump) = xdis::tool("harness_detects_a_flipped_rex_bit") else { return };
    let m = Mem::b(R12, 0x10);
    let mut a = Asm::new();
    a.load(Size::S64, Rax, m);
    let mut code = a.finish().unwrap();
    let want = format!("mov rax,{}", sized("QWORD", m));
    assert_eq!(xdis::disassemble_many(&objdump, &[&code])[0][0].text(), want);
    code[0] ^= 0x01; // REX.B: r12 -> rsp
    let got = xdis::disassemble_many(&objdump, &[&code])[0][0].text();
    assert_ne!(got, want);
    assert_eq!(got, "mov rax,QWORD PTR [rsp+0x10]");
}
