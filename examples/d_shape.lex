type Shape =
    Circle({ radius :: Float })
  | Rect({ width :: Float, height :: Float })

fn area(s :: Shape) -> Float
  examples {
    area(Rect({ width: 3.0, height: 4.0 })) => 12.0,
    area(Rect({ width: 0.0, height: 5.0 })) => 0.0,
  }
{
  match s {
    Circle({ radius }) => 3.14159 * radius * radius,
    Rect({ width, height }) => width * height,
  }
}
