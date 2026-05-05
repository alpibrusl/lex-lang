spec clamp {
  forall x :: Int, lo :: Int, hi :: Int where lo <= hi:
    let r := clamp(x, lo, hi)
    (r >= lo) and (r <= hi)
}
