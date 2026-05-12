fn factorial(n :: Int) -> Int
  examples {
    factorial(0) => 1,
    factorial(1) => 1,
    factorial(5) => 120
  }
{
  match n {
    0 => 1,
    _ => n * factorial(n - 1),
  }
}

