import "std.str" as str
import "std.result" as result

type ParseError = Empty | NotNumber

fn parse_int(s :: Str) -> Result[Int, ParseError] {
  if str.is_empty(s) {
    Err(Empty)
  } else {
    match str.to_int(s) {
      Some(n) => Ok(n),
      None    => Err(NotNumber),
    }
  }
}

fn double_input(s :: Str) -> Result[Int, ParseError] {
  parse_int(s) |> result.map(fn (n :: Int) -> Int { n * 2 })
}
