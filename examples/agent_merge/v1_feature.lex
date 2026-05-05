# "feature" branch: an agent rewrote `clamp` to use min/max-style
# arithmetic instead of nested matches. Same behavior, different
# AST — the merge engine sees a ModifyModify because the body
# changed.

fn min2(a :: Int, b :: Int) -> Int { match a < b { true => a, false => b } }
fn max2(a :: Int, b :: Int) -> Int { match a > b { true => a, false => b } }

fn clamp(x :: Int, lo :: Int, hi :: Int) -> Int {
  min2(max2(x, lo), hi)
}
