pub fn wiki_help_text() -> &'static str {
    "Writestead workflow guide:

Vault layout:
- raw/ stores source files for deterministic extraction.
- wiki/ stores markdown knowledge pages.
- wiki/entities/, wiki/concepts/, wiki/sources/, wiki/analyses/ hold content pages by type.
- wiki/index.md is the navigation index.
- wiki/log.md is the change log.

Ingest workflow:
1) raw_list to discover source files.
2) raw_read to extract paginated text.
3) agent reasoning outside writestead.
4) wiki_write for new pages, wiki_edit for targeted updates.
5) wiki_lint to verify structural health.
6) wiki_lint { fix: true } if autofixable issues exist.
7) wiki_sync after write or edit operations.

Conventions:
- Every wiki page must have YAML frontmatter.
- Page types: source, entity, concept, analysis.
- Use [[wikilinks]] for cross references.
- No emojis in page content or logs.

Frontmatter template:
---
title: Example
type: entity
created: YYYY-MM-DD
updated: YYYY-MM-DD
tags: [tag1, tag2]
---

Operational notes:
- raw_list offset is 0-indexed by item.
- raw_read offset is 1-indexed by line.
- wiki_list offset is 0-indexed by item.
- wiki_read offset is 1-indexed by line.
- Use paginated reads for long files to keep tool calls small."
}
