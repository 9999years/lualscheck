# CLI diagnostic client for `lua-language-server`

[`lua-language-server`][luals] contains a nice type checker and linter, but no
way to use it. You can run `lua-language-server --check my-project-dir`, but
it'll write the diagnostics in a clumsy JSON format in some random log
directory. It also writes diagnostics for all library files, not just files
under your project root.

The latter problem could be solved with [my PR to add a `--check_out_path` CLI
argument][check_out_path], but that doesn't solve the issue of clumsy JSON
diagnostics.

`lualscheck` runs `lua-language-server --check your-project-dir`, reads the
diagnostics, formats them nicely, and prints out the ones from files under your
project directory.

You can filter the level of diagnostics to show and the level of diagnostics to
error on.

[luals]: https://github.com/LuaLS/lua-language-server
[check_out_path]: https://github.com/LuaLS/lua-language-server/pull/2364


```ShellSession
$ lualscheck
Diagnosis complete, 10 problems found, see /Users/wiggles/.cache/lua-language-server/log/check.json

lua/broot/init.lua:67:27 [miss-symbol]
    error: Missed symbol `}`.

Error:   × lua-language-server found 1 problems
```
