# Build the handbook

The [handbook source](handbook/index.md) is MyST Markdown. Sphinx renders it into
HTML with local navigation, search, styles, scripts, and figures. The source
also remains readable in a text editor. Rendering requires no SDR hardware,
native radio stack, Rust dependencies, or original vendor PDFs.

The build uses **Sphinx 9.2.0**, **MyST-Parser 5.1.0**, and
**Furo 2025.12.19**. Their resolved dependencies are pinned in
[requirements.txt](requirements.txt). Use Python 3.13 or newer.

## Catalog consistency

The committed [fact reference](handbook/fact-catalog.md) is generated from
[tracked JSON and its schema](knowledge/README.md). A complete documentation check
includes these host-only Rust commands, run from the workspace root:

```sh
cargo run --locked -p bladerf-docs -- check
cargo run --locked -p bladerf-docs -- render --check
```

After a deliberate data edit and source review, run `render` without `--check`
and inspect the generated diff. The [tool guide](../tools/bladerf-docs/README.md)
states exactly what consistency validation establishes. Passing it does not
verify source truth or hardware behavior.

## HTML

Run from the workspace root. Installation needs package access;
subsequent builds use the installed tools and local source files.

```sh
python3 -m venv build/documentation-preview/venv
build/documentation-preview/venv/bin/python -m pip install -r docs/requirements.txt
build/documentation-preview/venv/bin/python -m pip check
build/documentation-preview/venv/bin/sphinx-build \
  -C -b html -n -W --keep-going \
  -d build/documentation-preview/doctrees/html \
  -D extensions=myst_parser \
  -D html_theme=furo \
  -D project='bladeRF micro xA9 handbook' \
  -D html_title='bladeRF micro xA9' \
  -D html_show_copyright=0 \
  -D author='' -D copyright='' -D language=en \
  docs/handbook build/documentation-preview/html
```

`-C` uses no Python configuration file. `-n -W` enables strict reference warnings
and fails the build on warnings. Sphinx's
[command reference](https://www.sphinx-doc.org/en/master/man/sphinx-build.html)
describes these options.

Serve the generated HTML over loopback HTTP for complete search behavior:

```sh
build/documentation-preview/venv/bin/python -m http.server \
  --bind 127.0.0.1 --directory build/documentation-preview/html 8000
```

Then open `http://127.0.0.1:8000/`. This serves an existing build locally; it
does not publish a website. Stop the foreground server with Ctrl-C.
Directly opening `index.html` also supports reading, but browser restrictions
on filesystem fetches can prevent search excerpts from loading.

## Text and other exports

Use the same options, changing `-b html` to `-b text`, the doctree directory to
`build/documentation-preview/doctrees/text`, and the output directory to
`build/documentation-preview/text`. This produces a plain-text reading edition
of the handbook.

HTML and text are the supported outputs described here. EPUB needs publication
metadata and packaging configuration. PDF export needs an appropriate backend
and toolchain. Both formats require their own layout checks.

## Offline reading

Styles, scripts, navigation and figures are included in the generated HTML.
On narrow screens, scroll tables horizontally and open a diagram's image link
to enlarge its labels.

External citations remain public source links. Reading the authored handbook
offline does not require them, but following a citation to an original manual
needs network access or a separately obtained copy. Build outputs are ignored
by Git; the source and catalog contain the files needed to render the handbook.
