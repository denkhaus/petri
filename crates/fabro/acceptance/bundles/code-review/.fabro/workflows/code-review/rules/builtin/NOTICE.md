# Built-in rule library attribution

Except for `repository/instructions.yaml`, the rule packs in this directory
are ported from Alibaba OpenCodeReview (OCR):

- Source: https://github.com/alibaba/open-code-review
- Files: `internal/config/rules/rule_docs/*.md` (rule content) and
  `internal/config/rules/system_rules.json` (path map)
- Commit: `89ec55b14442c9f2601fb55b5f554fb6fabbe2c7`
- License: Apache License 2.0 (see the `LICENSE` file in this directory)
- Copyright: alibaba/open-code-review Contributors

Changes made in the port:

- Each Markdown rule document became one YAML rule pack; its `#### `
  sections became individual checks with stable IDs and one of this
  workflow's closed finding categories.
- A leading "Review Principles" section or preamble became the pack's
  `description`.
- OCR's product-specific tool names (`file_read`, `code_search`) were
  replaced with this workflow's read-only exploration language, and a
  reference to OCR's default path filter was reworded.
- OCR's `default_rule` semantics are preserved by the engine: the `default`
  pack applies only to files no other built-in pack matches. OCR's `.m`
  content sniff (MATLAB vs Objective-C) is ported into the engine and
  selects between `language.matlab` and `language.objective-c`.
- Unlike OCR, matching repository rules do not replace built-in rules by
  default: repository rules merge unless they declare `mode: override`.
