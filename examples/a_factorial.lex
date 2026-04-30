fn factorial(n :: Int) -> Int {
  match n {
    0 => 1,
    _ => n * factorial(n - 1),
  }
}
