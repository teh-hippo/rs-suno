# Cloud agents

How we use the GitHub Copilot cloud agent (`copilot-swe-agent`) on rs-suno.

## When to use one

Credential-free, verifiable work: pure `suno-core` modules, documentation, and refactors. Never use a cloud agent for token-bound work or anything that needs real-player verification; that stays local. The session token is never handed to a cloud runner.

## Dispatch

1. Create a self-contained issue. The agent reads the repository and `AGENTS.md`, not our local notes, so include the goal, context, constraints, acceptance criteria, writing style, and the specific files in scope.
2. Assign the agent through GraphQL (the `gh` CLI has no native command for this):
   - Find the bot id from `suggestedActors(capabilities:[CAN_BE_ASSIGNED])`; it is `copilot-swe-agent`.
   - Call `replaceActorsForAssignable(input:{assignableId, actorIds:[botId]})`.
3. The agent opens a draft pull request on a `copilot/*` branch and works in an Actions runner configured by `.github/workflows/copilot-setup-steps.yml`.

## Review and integrate

- Every cloud pull request goes through our expert review loop (rubber-duck and code-review, plus deeper experts on hard work) until consensus.
- Integrate fast-forward only: reconcile on the branch if needed, rebase onto `main`, then `git merge --ff-only`.
- Feed recurring fixes back into `AGENTS.md` so future cloud output needs less rework.

## Runner

`copilot-setup-steps.yml` preinstalls the Rust toolchain, ffmpeg, and warms the build cache, so the agent starts from a green build.
