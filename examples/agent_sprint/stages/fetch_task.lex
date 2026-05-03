# Stage 1: read a Task from disk. The only effect is `[io]` —
# narrowed at the host invocation by `--allow-fs-read ./tasks`, so
# this stage cannot read anything outside the tasks directory even
# though it has the bare `[io]` capability.

import "../types" as t
import "std.io"   as io
import "std.json" as json

fn run(path :: Str) -> [io] Result[t.Task, Str] {
  match io.read(path) {
    Ok(s)  => json.parse(s),
    Err(e) => Err(e),
  }
}
