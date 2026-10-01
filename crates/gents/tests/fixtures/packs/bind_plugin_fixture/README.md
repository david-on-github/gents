# bind_plugin_fixture

A fixture `plugins` pack for `bind_dir` tests: its one plugin, `list_files`,
lists the file names directly under whatever directory its caller binds it
to. Used by `gents-cli`'s `gents plugin run --bind-dir` and `gents pack
test` bind-case tests.
