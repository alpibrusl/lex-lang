# Initial implementation of `clamp` — the version on `main`
# before the agent's branch diverges. Both the "feature"
# branch and `main` will modify this body in incompatible
# ways, producing a ModifyModify conflict the agent has to
# resolve.

fn clamp(x :: Int, lo :: Int, hi :: Int) -> Int {
  match x < lo {
    true => lo,
    false => match x > hi {
      true => hi,
      false => x,
    },
  }
}
