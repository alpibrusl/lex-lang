//! Native implementations for Core stages.
//!
//! For Phase 2 we don't ship a Cranelift JIT. Instead, the perf path is
//! a registry of hand-written Rust functions that the runtime dispatches
//! to when Lex calls a Core stage by name. From Lex's perspective, the
//! call goes through the standard `EffectCall` mechanism with kind
//! `core` — semantically identical to any other stage call, but the
//! body is compiled Rust.
//!
//! `matmul` is the canonical demo: §13.7 #1 wants 1024×1024 in <100ms.
//! The implementation here does cache-blocked tiled matmul on a flat
//! `Vec<f64>` and routinely beats that bound on commodity hardware.

use indexmap::IndexMap;
use lex_bytecode::Value;

/// A Core stage's compiled form: a Rust closure taking unwrapped args.
pub type NativeFn = std::sync::Arc<dyn Fn(&[Value]) -> Result<Value, String> + Send + Sync>;

/// Registry of native Core implementations, looked up by `op` name.
#[derive(Default, Clone)]
pub struct NativeRegistry {
    pub fns: IndexMap<String, NativeFn>,
}

impl NativeRegistry {
    pub fn new() -> Self { Self::default() }

    pub fn with_defaults() -> Self {
        let mut r = Self::new();
        r.register("matmul", std::sync::Arc::new(|args: &[Value]| native_matmul(args)));
        r.register("dot", std::sync::Arc::new(|args: &[Value]| native_dot(args)));
        r
    }

    pub fn register(&mut self, name: impl Into<String>, f: NativeFn) {
        self.fns.insert(name.into(), f);
    }

    pub fn dispatch(&self, op: &str, args: &[Value]) -> Option<Result<Value, String>> {
        self.fns.get(op).map(|f| f(args))
    }
}

/// Matrix value layout: `Record { rows, cols, data: List[Float] }`,
/// row-major. We unwrap to a flat `Vec<f64>` for the hot loop.
fn unpack_matrix(v: &Value) -> Result<(usize, usize, Vec<f64>), String> {
    let rec = match v {
        Value::Record(r) => r,
        other => return Err(format!("expected Record matrix, got {other:?}")),
    };
    let rows = rec.get("rows").ok_or("missing field rows")?;
    let cols = rec.get("cols").ok_or("missing field cols")?;
    let data = rec.get("data").ok_or("missing field data")?;
    let r = match rows { Value::Int(n) => *n as usize, _ => return Err("rows not Int".into()) };
    let c = match cols { Value::Int(n) => *n as usize, _ => return Err("cols not Int".into()) };
    let buf: Vec<f64> = match data {
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::Float(f) => out.push(*f),
                    Value::Int(n) => out.push(*n as f64),
                    other => return Err(format!("matrix data: expected Float, got {other:?}")),
                }
            }
            out
        }
        _ => return Err("matrix data not List".into()),
    };
    if buf.len() != r * c {
        return Err(format!("matrix data has {} elements but rows*cols = {}", buf.len(), r * c));
    }
    Ok((r, c, buf))
}

fn pack_matrix(rows: usize, cols: usize, data: Vec<f64>) -> Value {
    let mut rec = IndexMap::new();
    rec.insert("rows".into(), Value::Int(rows as i64));
    rec.insert("cols".into(), Value::Int(cols as i64));
    rec.insert("data".into(), Value::List(data.into_iter().map(Value::Float).collect()));
    Value::Record(rec)
}

/// Native matmul. C = A · B where A is M×K, B is K×N, C is M×N.
/// Uses the `matrixmultiply` crate's hand-tuned SIMD f64 kernel
/// (`dgemm`) for the hot inner loop — hits ~10 GFLOPS on commodity
/// CPUs in release builds, keeping 1024×1024 well under §13.7's
/// 100ms target.
fn native_matmul(args: &[Value]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(format!("matmul: expected 2 args, got {}", args.len()));
    }
    let (m, k1, a) = unpack_matrix(&args[0])?;
    let (k2, n, b) = unpack_matrix(&args[1])?;
    if k1 != k2 {
        return Err(format!("matmul: inner dim mismatch, {}×{} · {}×{}", m, k1, k2, n));
    }
    let mut c = vec![0.0_f64; m * n];
    // SAFETY: A is M×K, B is K×N, C is M×N, all contiguous row-major.
    // dgemm requires non-aliasing inputs, which we satisfy: a and b
    // are owned by us, c is freshly zeroed.
    unsafe {
        matrixmultiply::dgemm(
            m, k1, n,
            1.0,
            a.as_ptr(), k1 as isize, 1,    // A: row-stride K1, col-stride 1
            b.as_ptr(), n as isize, 1,     // B: row-stride N, col-stride 1
            0.0,
            c.as_mut_ptr(), n as isize, 1, // C: row-stride N, col-stride 1
        );
    }
    Ok(pack_matrix(m, n, c))
}

/// `dot(a, b)` for two equal-length flat Float lists. Useful as a
/// smaller native test case.
fn native_dot(args: &[Value]) -> Result<Value, String> {
    let extract = |v: &Value| -> Result<Vec<f64>, String> {
        match v {
            Value::List(items) => items.iter().map(|x| match x {
                Value::Float(f) => Ok(*f),
                Value::Int(n) => Ok(*n as f64),
                other => Err(format!("dot: expected Float, got {other:?}")),
            }).collect(),
            other => Err(format!("dot: expected List, got {other:?}")),
        }
    };
    let a = extract(args.first().ok_or("dot: missing arg 0")?)?;
    let b = extract(args.get(1).ok_or("dot: missing arg 1")?)?;
    if a.len() != b.len() {
        return Err(format!("dot: length mismatch {} vs {}", a.len(), b.len()));
    }
    let mut s = 0.0;
    for i in 0..a.len() { s += a[i] * b[i]; }
    Ok(Value::Float(s))
}

/// Construct a Lex-side matrix value from a flat `Vec<f64>` and dims.
pub fn make_matrix(rows: usize, cols: usize, data: Vec<f64>) -> Value {
    pack_matrix(rows, cols, data)
}
