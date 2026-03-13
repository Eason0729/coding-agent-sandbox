# Plan Folder

There are multiple human-reviewed markdown files including:
1. Project overview
2. Responsibility of each module
3. Implementation detail of each module

**No code without corresponding markdown spec**

## Impossible Plan

The plan is IMPOSSIBLE in following cases:
1. The interface/protocol miss important information that client/server cannot proceed request.
2. The interface/protocol miss important functionality that server should take responsibility for.
3. The fundamental technology lack the ability to do so.
4. The interface/protocol does not match between markdown files.

In the case of impossible plan, try to:
1. Work through the workflow of such case.
2. Make human/user understand why it's impossible.
3. Modify the plan and ask human to review it.

Additionally, if low-level program bugs/inability is encountered, consult to man page.

**Human/User are professional software engineers, also consult user for lack of low-level understanding to system**

## Rust Coding guideline

1. Always unit test if possible
2. Always check if it compile with `cargo check` or `cargo build`.
  - `cargo build --release` is usually unnecessary.
3. Always format the file with `cargo fmt`
