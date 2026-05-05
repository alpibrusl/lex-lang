# Meanwhile on `main`: a different agent (or the human
# maintainer) rewrote `clamp` to add an early-return guard for
# the degenerate `lo > hi` case. Different body again — the
# merge will see this as ours-side of the same ModifyModify.

fn clamp(x :: Int, lo :: Int, hi :: Int) -> Int {
  match lo > hi {
    true => x,
    false => match x < lo {
      true => lo,
      false => match x > hi {
        true => hi,
        false => x,
      },
    },
  }
}
