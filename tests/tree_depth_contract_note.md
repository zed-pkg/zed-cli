# tree_depth flags2env contract repair

This branch changes only the `commands.tree.flags.tree_depth` type token from the unsupported `int` spelling to the canonical `integer` spelling used elsewhere in `.cli-flags.toml`.

The temporary repair workflow removes itself after applying that one-line change so the final pull request stays free of mutation-only CI helpers.
