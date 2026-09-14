# Skills fixtures

Versioned inputs and expectations for readiness item 9c (milestone C3):
Fabro skill directories, precedence, loading, prompt and tool behavior.

## Layout

- `home/skills`: the configured skills directory (`$FABRO_HOME/skills`).
  `greet` here loses to every later directory; `home-only` exists only here.
- `repo/.fabro/skills`: the repository's `.fabro/skills`. `greet` here beats
  the home copy and loses to `repo/skills`; `project-only` exists only here.
- `repo/skills`: the repository's `skills`. Its `greet` wins. `repo-only`
  and `cleanup` exist only here. Three files are not skills: `no-frontmatter`
  (no `---` block), `no-name` (a block with no `name:`), `unterminated` (a
  block that never closes). Fabro and Pebble skip them; Petri records a
  `attractor.skills.warning` for each.
- `workflow/skills`: a directory a workflow names with `[run.agent] skills`
  (a Petri extension). Its `greet` beats the repository's.
- `expected/`: the reference prompt section and skill tool definition for
  the `use_skill` vocabulary (Fabro's own, Codex) and the `Skill` vocabulary
  (Claude 5, Kimi Code), for a run that sees `home` and `repo`.

Tests copy `repo` into a Git repository and point `FABRO_HOME` (or the
`attractor_steps::skills::FabroHome` capability) at `home`.

## Where the expectations come from

The pinned Fabro (`crates/fabro/corpus-pin.txt`) was not run against these
fixtures: driving its `run` command needs a Fabro server, a Fabro provider
configuration that points at the twins and a Fabro build from the corpus.
The expectations are derived from Fabro's source at the pin instead:

- directory order: `lib/components/fabro-agent/src/skills.rs`
  `default_skill_dirs` and `lib/components/fabro-agent/src/session.rs`
  `initialize` (the configured directory from `fabro_util::Home::skills_dir`,
  then `<root>/.fabro/skills`, then `<root>/skills`);
- precedence and parsing: `discover_skills` and `parse_skill` in the same
  file (later directories override earlier names; a file that does not parse
  is skipped);
- the prompt section: `format_skills_prompt_section`;
- the tool: `make_use_skill_tool_for_vocabulary` (argument names and schema
  per vocabulary, `NativeTool::UseSkill` names in `native_tool.rs`).

Pebble's `crates/pebble-coding-agent/src/skills.rs` and `tools/skill.rs` at
the pinned revision carry the same text and schemas; the tests assert the
files here against what the twins received from Petri.
