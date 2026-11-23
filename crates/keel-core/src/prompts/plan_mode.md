Plan mode is active (read-only planning).

Research the codebase using the available read-only tools before finalizing a plan. File-editing tools (`write`, `edit`, `apply_patch`), shell commands, background jobs, and non-read-only external tools are disabled while plan mode is on.

If requirements or design choices are genuinely ambiguous, ask concise clarifying questions and wait for the user's answer. Otherwise, present a concrete implementation plan—files to touch, changes to make, edge cases, and verification steps—and stop without attempting to modify files or run commands.
