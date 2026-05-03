# Stage 4: persist the agent's output keyed by task id and emit a
# structured log line. Effect set is `[kv, fs_write, log]` — the
# `fs_write` comes from `kv.open` declaring it (the embedded sled
# store writes its own files), so the host's `--allow-fs-write
# ./sprint.db` scope gates *both* the kv store creation and any
# stray `io.write` calls anywhere else in the program.

import "../types"  as t
import "std.kv"    as kv
import "std.bytes" as bytes
import "std.log"   as log
import "std.str"   as str

fn run(
  db_path :: Str,
  task    :: t.Task,
  output  :: Str,
  verdict :: Bool,
) -> [kv, fs_write, log] Result[Str, Str] {
  let tag := if verdict { "PASS" } else { "FAIL" }
  match kv.open(db_path) {
    Ok(db) => match kv.put(db, task.id, bytes.from_str(output)) {
      Ok(_) => {
        log.info(str.concat(str.concat(tag, ": "), task.id))
        Ok(str.concat(str.concat(task.id, " => "), tag))
      },
      Err(e) => Err(e),
    },
    Err(e) => Err(e),
  }
}
