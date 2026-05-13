import "std.net" as net
import "std.iter" as iter
import "std.str" as str
import "std.int" as int
import "std.map" as map

# Streaming HTTP responses (#375).
#
# Each route demonstrates a different `ResponseBody` variant:
#   - `/`     — plain `BodyStr`, the existing eager-string shape
#   - `/sse`  — `BodyStream`, chunks emitted under chunked transfer-encoding
#   - `/blob` — `BodyBytes`, binary chunks for downloads
#
# Verify on the wire with:
#
#     curl -i --no-buffer http://127.0.0.1:8088/sse
#     # → HTTP/1.1 200 OK
#     #   Transfer-Encoding: chunked
#     #   data: tick 0
#     #   data: tick 1
#     #   data: tick 2

fn sse_chunks() -> List[Str] {
  let mk := fn (n :: Int) -> Str {
    str.concat(str.concat("data: tick ", int.to_str(n)), "\n\n")
  }
  [mk(0), mk(1), mk(2)]
}

fn handler(req :: Request) -> Response {
  match req.path {
    "/" => {
      status:  200,
      body:    BodyStr("streaming demo: try /sse or /blob"),
      headers: map.from_list([("content-type", "text/plain")]),
    },
    "/sse" => {
      status:  200,
      body:    BodyStream(iter.from_list(sse_chunks())),
      headers: map.from_list([("content-type", "text/event-stream")]),
    },
    "/blob" => {
      status:  200,
      body:    BodyBytes(iter.from_list([
        # Three 4-byte binary chunks. The runtime concatenates them
        # under chunked transfer-encoding; a client receives all 12
        # bytes intact.
        [104, 105, 33, 10],            # "hi!\n"
        [98, 121, 101, 10],            # "bye\n"
        [102, 105, 110, 10],           # "fin\n"
      ])),
      headers: map.from_list([("content-type", "application/octet-stream")]),
    },
    _ => {
      status:  404,
      body:    BodyStr("not found"),
      headers: map.from_list([("content-type", "text/plain")]),
    },
  }
}

fn main() -> [net] Unit {
  net.serve_fn(8088, handler)
}
