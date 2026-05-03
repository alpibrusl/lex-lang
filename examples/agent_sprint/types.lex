# Shared types for the agent-sprint demo. Both the orchestrator and
# every stage import this file so the type checker sees a single
# nominal `Task` flowing through the pipeline rather than four
# structurally-equal-but-distinct shapes.

type Task = {
  id :: Str,
  prompt :: Str,
  args :: List[Str],
  criteria :: Str,
}
