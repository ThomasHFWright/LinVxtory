//! CPU execution limited to the synthetic raw-load fixtures, not a shader emulator.
use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RawLoadTrace {
    pub words: Vec<u32>,
    /// Executed calls: byte address and component mask.
    pub loads: Vec<(u32, u8)>,
}

#[derive(Clone, Debug)]
enum Value {
    Bits(u32),
    Aggregate(Vec<Value>),
    Undef,
}

impl Value {
    fn bits(&self) -> u32 {
        match self {
            Self::Bits(x) => *x,
            x => panic!("raw-load fixture expected defined bits, got {x:?}"),
        }
    }
}

impl Module {
    /// `read` supplies one raw-view dword, including its out-of-bounds behavior.
    /// Component offsets within one call are widened, never wrapped to zero.
    pub(crate) fn trace_raw_loads(
        &self,
        entry: usize,
        address: u32,
        read: impl Fn(u64) -> u32,
    ) -> RawLoadTrace {
        fn get(m: &Module, values: &HashMap<u32, Value>, v: V) -> Value {
            match v {
                V::Inst(i) => values.get(&i).expect("defined fixture value").clone(),
                V::Const(c) => match &m.consts[c as usize].1 {
                    Const::Int(n) => Value::Bits(*n as u32),
                    Const::Float(bits) => Value::Bits(*bits),
                    Const::Undef => Value::Undef,
                    Const::Aggregate(fields) => Value::Aggregate(
                        fields
                            .iter()
                            .map(|&c| get(m, values, V::Const(c)))
                            .collect(),
                    ),
                },
                _ => panic!("unsupported raw-load fixture value {v:?}"),
            }
        }
        let mut values = HashMap::new();
        let mut trace = RawLoadTrace {
            words: Vec::new(),
            loads: Vec::new(),
        };
        let (mut block, mut previous) = (entry, usize::MAX);
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(
                steps < 100_000,
                "raw-load fixture exceeded execution budget"
            );
            let b = &self.blocks[block];
            let phis: Vec<_> = b
                .phis
                .iter()
                .map(|&i| {
                    let Op::Phi(edges) = &self.insts[i as usize].op else {
                        panic!("phi")
                    };
                    let &(v, _) = edges.iter().find(|e| e.1 == previous).expect("phi edge");
                    (i, get(self, &values, v))
                })
                .collect();
            values.extend(phis);
            for &i in &b.body {
                steps += 1;
                assert!(
                    steps < 100_000,
                    "raw-load fixture exceeded execution budget"
                );
                let value = |v| get(self, &values, v);
                let bits = |v| value(v).bits();
                let inst = &self.insts[i as usize];
                let result = match &inst.op {
                    Op::Bin(op, a, b) => {
                        assert!(matches!(self.types[inst.ty.0 as usize], TypeDef::Int(32)));
                        let (a, b) = (bits(*a), bits(*b));
                        Value::Bits(match op {
                            Bin::Add => a.wrapping_add(b),
                            Bin::Sub => a.wrapping_sub(b),
                            Bin::Mul => a.wrapping_mul(b),
                            Bin::And => a & b,
                            Bin::Or => a | b,
                            Bin::Xor => a ^ b,
                            Bin::Shl => a.checked_shl(b).expect("defined shift"),
                            Bin::LShr => a.checked_shr(b).expect("defined shift"),
                            _ => panic!("unsupported raw-load fixture binary {op:?}"),
                        })
                    }
                    Op::Cmp(p, a, b) => {
                        let (a, b) = (bits(*a), bits(*b));
                        Value::Bits(u32::from(match *p {
                            pred::EQ => a == b,
                            pred::NE => a != b,
                            pred::IUGT => a > b,
                            pred::IUGE => a >= b,
                            pred::IULT => a < b,
                            pred::IULE => a <= b,
                            _ => panic!("unsupported raw-load fixture predicate {p}"),
                        }))
                    }
                    Op::Select(c, a, b) => value(if bits(*c) != 0 { *a } else { *b }),
                    Op::Cast(Cast::Bitcast, v) => {
                        for ty in [inst.ty, self.type_of(*v)] {
                            assert!(matches!(
                                self.types[ty.0 as usize],
                                TypeDef::Int(32) | TypeDef::Float
                            ));
                        }
                        Value::Bits(bits(*v))
                    }
                    Op::Extract(v, k) => {
                        let Value::Aggregate(fields) = value(*v) else {
                            panic!("aggregate")
                        };
                        fields[*k as usize].clone()
                    }
                    Op::Call(f, args) => {
                        let name = self.funcs[*f as usize].name.as_str();
                        match name {
                            "test.address" => {
                                assert!(args.is_empty());
                                Value::Bits(address)
                            }
                            "test.result" => {
                                assert_eq!(args.len(), 1);
                                trace.words.push(bits(args[0]));
                                Value::Undef
                            }
                            "dx.op.rawBufferLoad.f32" | "dx.op.rawBufferLoad.i32" => {
                                assert_eq!(args.len(), 6);
                                assert_eq!(bits(args[0]), 139);
                                assert!(
                                    matches!(value(args[1]), Value::Undef),
                                    "synthetic handle only"
                                );
                                assert!(
                                    matches!(value(args[3]), Value::Undef),
                                    "raw byte address only"
                                );
                                assert_eq!(bits(args[5]), 4);
                                let (at, mask) = (bits(args[2]), bits(args[4]));
                                assert!(mask > 0 && mask <= 15);
                                assert_eq!(at % 4, 0, "aligned synthetic address");
                                trace.loads.push((at, mask as u8));
                                let mut fields: Vec<_> = (0..4)
                                    .map(|k| {
                                        if mask & (1 << k) != 0 {
                                            Value::Bits(read(u64::from(at) + 4 * k))
                                        } else {
                                            Value::Undef
                                        }
                                    })
                                    .collect();
                                fields.push(Value::Undef); // Residency status is not observed.
                                Value::Aggregate(fields)
                            }
                            _ => panic!("unsupported raw-load fixture call {name}"),
                        }
                    }
                    op => panic!("unsupported raw-load fixture instruction {op:?}"),
                };
                values.insert(i, result);
            }
            previous = block;
            block = match b.term.expect("fixture terminator") {
                Term::Br(next) => next,
                Term::CondBr(c, yes, no) => {
                    if get(self, &values, c).bits() != 0 {
                        yes
                    } else {
                        no
                    }
                }
                Term::Ret => return trace,
            };
        }
    }
}
