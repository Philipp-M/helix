# Languages

Language-specific settings and settings for language servers are configured
in `languages.toml` files.

## `languages.toml` files

There are three possible `languages.toml` files. The first is compiled into
Helix and lives in the [Helix repository](https://github.com/helix-editor/helix/blob/master/languages.toml).
This provides the default configurations for languages and language servers.

You may define a `languages.toml` in your [configuration directory](./configuration.md)
which overrides values from the built-in language configuration. For example
to disable auto-LSP-formatting in Rust:

```toml
# in <config_dir>/helix/languages.toml

[[language]]
name = "rust"
auto-format = false
```

Language configuration may also be overridden local to a project by creating
a `languages.toml` file under a `.helix` directory. Its settings will be merged
with the language configuration in the configuration directory and the built-in
configuration.

## Language configuration

Each language is configured by adding a `[[language]]` section to a
`languages.toml` file. For example:

```toml
[[language]]
name = "mylang"
scope = "source.mylang"
injection-regex = "^mylang$"
file-types = ["mylang", "myl"]
comment-token = "#"
indent = { tab-width = 2, unit = "  " }
formatter = { command = "mylang-formatter" , args = ["--stdin"] }
language-servers = [ "mylang-lsp" ]
```

These configuration keys are available:

| Key                   | Description                                                   |
| ----                  | -----------                                                   |
| `name`                | The name of the language                                      |
| `language-id`         | The language-id for language servers, checkout the table at [TextDocumentItem](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocumentItem) for the right id |
| `scope`               | A string like `source.js` that identifies the language. Currently, we strive to match the scope names used by popular TextMate grammars and by the Linguist library. Usually `source.<name>` or `text.<name>` in case of markup languages |
| `injection-regex`     | regex pattern that will be tested against a language name in order to determine whether this language should be used for a potential [language injection][treesitter-language-injection] site. |
| `file-types`          | The filetypes of the language, for example `["yml", "yaml"]`. Extensions and full file names are supported.  |
| `shebangs`            | The interpreters from the shebang line, for example `["sh", "bash"]` |
| `roots`               | A set of marker files to look for when trying to find the workspace root. For example `Cargo.lock`, `yarn.lock` |
| `auto-format`         | Whether to autoformat this language when saving               |
| `diagnostic-severity` | Minimal severity of diagnostic for it to be displayed. (Allowed values: `Error`, `Warning`, `Info`, `Hint`) |
| `comment-token`       | The token to use as a comment-token                           |
| `indent`              | The indent to use. Has sub keys `tab-width` and `unit`        |
| `language-servers`    | The Language Servers used for this language. See below for more information   |
| `config`              | Language Server configuration                                 |
| `grammar`             | The tree-sitter grammar to use (defaults to the value of `name`) |
| `formatter`           | The formatter for the language, it will take precedence over the lsp when defined. The formatter must be able to take the original file as input from stdin and write the formatted file to stdout |
| `max-line-length`     | Maximum line length. Used for the `:reflow` command           |

In case multiple language servers are specified, it's often useful to only enable/disable certain language-server features for these language servers.
For example `efm-lsp-prettier` (see in the next section for an example configuration for typescript) is used only with a formatting command `prettier`
but everything else should be handled by the `typescript-language-server` (configured by default)
The language configuration for typescript could look like this:

```toml
[[language]]
name = "typescript"
language-servers = [ { name = "efm-lsp-prettier", only-features = [ "format" ] }, "typescript-language-server" ]
```

or equivalent:

```toml
[[language]]
name = "typescript"
language-servers = [ { name = "typescript-language-server", except-features = [ "format" ] }, "efm-lsp-prettier" ]
```

Each requested LSP feature is priorized in the order of the `language-servers` array.
For example the first `goto-definition` supported language server (in this case `typescript-language-server`) will be taken for the relevant LSP request (command `goto_definition`).
If no `except-features` or `only-features` is given all features for the language server are enabled.

The list of supported features are:

- `format`
- `goto-definition`
- `goto-type-definition`
- `goto-reference`
- `goto-implementation`
- `signature-help`
- `hover`
- `document-highlight`
- `completion`
- `code-action`
- `document-symbols`
- `workspace-symbols`
- `diagnostics`
- `rename-symbol`

## Language Server configuration

Language servers are configured separately in the table `language-server` in the same file as the languages [`languages.toml`][languages.toml]

For example:

```toml
[langauge-server.mylang-lsp]
command = "mylang-lsp"
args = ["--stdio"]
config = { provideFormatter = true }

[language-server.efm-lsp-prettier]
command = "efm-langserver"

[language-server.efm-lsp-prettier.config]
documentFormatting = true
languages = { typescript = [ { formatCommand ="prettier --stdin-filepath ${INPUT}", formatStdin = true } ] }
```

These are the available options for a language server.

| Key                   | Description                                                                              |
| ----                  | -----------                                                                              |
| `command`             | The name or path of the language server binary to execute. Binaries must be in `$PATH`   |
| `args`                | A list of arguments to pass to the language server binary                                |
| `config`              | LSP initialization options                               |
| `timeout`             | The maximum time a request to the language server may take, in seconds. Defaults to `20` |

A `format` sub-table within `config` can be used to pass extra formatting options to
[Document Formatting Requests](https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/specification-3-16.md#document-formatting-request--leftwards_arrow_with_hook).
For example with typescript:

```toml
[langauge-server.typescript-language-server]
# pass format options according to https://github.com/typescript-language-server/typescript-language-server#workspacedidchangeconfiguration omitting the "[language].format." prefix.
config = { format = { "semicolons" = "insert", "insertSpaceBeforeFunctionParenthesis" = true } }
```

## Tree-sitter grammar configuration

The source for a language's tree-sitter grammar is specified in a `[[grammar]]`
section in `languages.toml`. For example:

```toml
[[grammar]]
name = "mylang"
source = { git = "https://github.com/example/mylang", rev = "a250c4582510ff34767ec3b7dcdd3c24e8c8aa68" }
```

Grammar configuration takes these keys:

| Key      | Description                                                              |
| ---      | -----------                                                              |
| `name`   | The name of the tree-sitter grammar                                      |
| `source` | The method of fetching the grammar - a table with a schema defined below |

Where `source` is a table with either these keys when using a grammar from a
git repository:

| Key    | Description                                               |
| ---    | -----------                                               |
| `git`  | A git remote URL from which the grammar should be cloned  |
| `rev`  | The revision (commit hash or tag) which should be fetched |
| `subpath` | A path within the grammar directory which should be built. Some grammar repositories host multiple grammars (for example `tree-sitter-typescript` and `tree-sitter-ocaml`) in subdirectories. This key is used to point `hx --grammar build` to the correct path for compilation. When omitted, the root of repository is used |

### Choosing grammars

You may use a top-level `use-grammars` key to control which grammars are
fetched and built when using `hx --grammar fetch` and `hx --grammar build`.

```toml
# Note: this key must come **before** the [[language]] and [[grammar]] sections
use-grammars = { only = [ "rust", "c", "cpp" ] }
# or
use-grammars = { except = [ "yaml", "json" ] }
```

When omitted, all grammars are fetched and built.

[treesitter-language-injection]: https://tree-sitter.github.io/tree-sitter/syntax-highlighting#language-injection
