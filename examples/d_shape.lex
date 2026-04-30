type Shape =
    Circle({ radius :: Float })
  | Rect({ width :: Float, height :: Float })

fn area(s :: Shape) -> Float {
  match s {
    Circle({ radius }) => 3.14159 * radius * radius,
    Rect({ width, height }) => width * height,
  }
}
