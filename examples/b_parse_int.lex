import "std.str" as str
import "std.result" as result

type ParseError = Empty | NotNumber

fn parse_int(s :: Str) -> Result[Int, ParseError]
  examples {
    parse_int("42") => Ok(42),
    parse_int("") => Err(Empty),
    parse_int("not a number") => Err(NotNumber),
  }
{
  if str.is_empty(s) {
    Err(Empty)
  } else {
    match str.to_int(s) {
      Some(n) => Ok(n),
      None    => Err(NotNumber),
    }
  }
}

fn double_input(s :: Str) -> Result[Int, ParseError]
  examples {
    double_input("21") => Ok(42),
    double_input("") => Err(Empty),
  }
{
  parse_int(s) |> result.map(fn (n :: Int) -> Int { n * 2 })
}
