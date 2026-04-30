# A REST API written in Lex.
#
# Run:
#   lex run --allow-effects net examples/weather_app.lex main
# Then in another terminal:
#   curl http://127.0.0.1:8080/weather/SF
#   curl http://127.0.0.1:8080/forecast/Paris
#   curl -XPOST http://127.0.0.1:8080/weather/Berlin   # 405
#
# Handler logic is one typed Lex function; routing is `match` on
# req.path and req.method. Weather data is mocked to keep the example
# dependency-free.

import "std.net" as net
import "std.str" as str
import "std.int" as int

type Request  = { body :: Str, method :: Str, path :: Str, query :: Str }
type Response = { body :: Str, status :: Int }

fn current_weather(city :: Str) -> Str {
  let temp_c := match city {
    "SF"     => 18,
    "Paris"  => 14,
    "Berlin" => 12,
    "Tokyo"  => 22,
    _        => 20,
  }
  let conditions := match city {
    "SF"     => "foggy",
    "Paris"  => "cloudy",
    "Berlin" => "rainy",
    "Tokyo"  => "clear",
    _        => "unknown",
  }
  let head := str.concat("{\"city\":\"", city)
  let head2 := str.concat(head, "\",\"temp_c\":")
  let head3 := str.concat(head2, int.to_str(temp_c))
  let head4 := str.concat(head3, ",\"conditions\":\"")
  let head5 := str.concat(head4, conditions)
  str.concat(head5, "\"}")
}

fn forecast(city :: Str) -> Str {
  let head := str.concat("{\"city\":\"", city)
  str.concat(head, "\",\"days\":[\"day1\",\"day2\",\"day3\"]}")
}

fn handle(req :: Request) -> Response {
  match req.method {
    "GET" => match str.strip_prefix(req.path, "/weather/") {
      Some(city) => { status: 200, body: current_weather(city) },
      None       => match str.strip_prefix(req.path, "/forecast/") {
        Some(city) => { status: 200, body: forecast(city) },
        None       => match req.path {
          "/health" => { status: 200, body: "{\"ok\":true}" },
          _         => { status: 404, body: "{\"error\":\"not found\"}" },
        },
      },
    },
    _ => { status: 405, body: "{\"error\":\"method not allowed\"}" },
  }
}

fn main() -> [net] Nil {
  net.serve(8080, "handle")
}
