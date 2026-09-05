//! Helpers behind the native list superinstructions: the
//! `list.sort_by` key comparator (#338) and the `list.par_map` worker
//! pool (#305). The `Op` arms that call them live in the dispatch
//! loop (`super::dispatch`).

use super::*;

/// Read `LEX_PAR_MAX_CONCURRENCY` (default = available CPU cores,
/// fallback 4). Capped at 64 so a malformed env var can't spawn an
/// unreasonable number of OS threads.
/// Order-defining comparator for `list.sort_by` keys (#338).
/// Same-typed Int / Float / Str pairs compare via their native
/// `Ord` / `PartialOrd`. Mixed-type or other key shapes compare
/// as Equal; combined with `Vec::sort_by`'s stability that
/// preserves the original element order — best-effort fallback
/// that never panics.
pub(super) fn compare_sort_keys(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

pub(super) fn par_max_concurrency() -> usize {
    let from_env = std::env::var("LEX_PAR_MAX_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0);
    let default = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    from_env.unwrap_or(default).min(64)
}

/// `list.par_map`'s runtime: spawn OS threads (capped by
/// `LEX_PAR_MAX_CONCURRENCY`), apply `closure` to each item, return
/// results in input order. Each worker runs a fresh `Vm` with
/// [`DenyAllEffects`] for #305 slice 1 — effectful closures fail
/// with `VmError::Effect`. Slice 2 will plumb a per-thread effect
/// handler split.
pub(super) fn par_map_run<'a>(
    program: &'a Program,
    closure: Value,
    items: Vec<Value>,
    worker_handlers: Vec<Box<dyn EffectHandler + Send>>,
    // Step budget for each worker VM. Without this, workers fall back to the
    // 10M default and ignore the caller's `--max-steps`, so a closure that does
    // real work (e.g. parsing a large payload) spuriously trips the step limit
    // inside a par_map even when the top-level run raised it. Inherit the
    // parent's limit so `--max-steps` applies uniformly.
    step_limit: u64,
) -> Result<Vec<Value>, VmError> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let n_workers = worker_handlers.len().min(items.len()).max(1);
    // Carve items into `n_workers` round-robin buckets so each
    // worker processes (indices, items) pairs and we can reassemble
    // in input order.
    let mut buckets: Vec<Vec<(usize, Value)>> = (0..n_workers).map(|_| Vec::new()).collect();
    for (i, v) in items.into_iter().enumerate() {
        buckets[i % n_workers].push((i, v));
    }
    let n_total: usize = buckets.iter().map(|b| b.len()).sum();
    let results: std::sync::Mutex<Vec<Option<Result<Value, String>>>> =
        std::sync::Mutex::new((0..n_total).map(|_| None).collect());

    // Pair each bucket with its pre-built handler so workers own
    // their handler outright — no shared mutable state across
    // worker threads.
    let mut worker_handlers = worker_handlers;
    worker_handlers.truncate(n_workers);
    type Pair = (Vec<(usize, Value)>, Box<dyn EffectHandler + Send>);
    let pairs: Vec<Pair> = buckets.into_iter().zip(worker_handlers).collect();

    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(pairs.len());
        for (bucket, handler) in pairs {
            let closure = closure.clone();
            let results = &results;
            handles.push(s.spawn(move || {
                // `Box<dyn EffectHandler + Send>` has implicit
                // `+ 'static`; that coerces to `+ 'a` because
                // `'static` outlives any `'a`. The `Send` bound is
                // auto-erased on the unsize coercion.
                let handler_for_vm: Box<dyn EffectHandler + 'a> = handler;
                let mut vm = Vm::with_handler(program, handler_for_vm);
                vm.set_step_limit(step_limit);
                for (idx, item) in bucket {
                    let r = vm
                        .invoke_closure_value(closure.clone(), vec![item])
                        .map_err(|e| format!("{e:?}"));
                    results.lock().unwrap()[idx] = Some(r);
                }
            }));
        }
        for h in handles {
            h.join().map_err(|_| ()).ok();
        }
    });

    let mut out = Vec::with_capacity(n_total);
    let inner = results.into_inner().unwrap();
    for r in inner {
        match r {
            Some(Ok(v)) => out.push(v),
            Some(Err(e)) => return Err(VmError::Effect(format!("par_map worker: {e}"))),
            None => return Err(VmError::Panic("par_map worker did not produce a result".into())),
        }
    }
    Ok(out)
}
