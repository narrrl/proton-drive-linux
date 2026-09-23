# Translating Proton Drive for Linux

The desktop app (`pdfs-app`), the tray (`pdfs-tray`) and the search prompt (`pdfs-prompt`) are
translated with GNU gettext in the `pdfs` text domain. The `pdfs` command-line tool and the daemon's
own messages stay in English, because scripts parse their output.

US English is the source language. The shipped catalogs are listed in [`po/LINGUAS`](../po/LINGUAS):
German, British English, Spanish, French, Italian, Dutch, Polish and Brazilian Portuguese.

## Picking a language

The apps follow the system locale (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`). Preferences →
General → Appearance → **Language** overrides that for the app, the tray and the prompt. The choice is
stored as `language` in `config.json` and takes effect the next time they start.

## Layout

| Path | Purpose |
| --- | --- |
| `crates/pdfs-gui/src/i18n.rs` | Locale setup and the `gettext`, `gettext_f`, `ngettext_f`, `pgettext` and `gettext_noop` helpers |
| `po/POTFILES.in` | Sources that `xgettext` scans |
| `po/pdfs.pot` | The template, generated from the sources |
| `po/<lang>.po` | One catalog per language |
| `po/update.sh` | Regenerates the template and merges it into every catalog |
| `po/build.sh <localedir>` | Compiles the catalogs to `<localedir>/<lang>/LC_MESSAGES/pdfs.mo` |

## Updating an existing translation

1. Run `po/update.sh`. It regenerates `po/pdfs.pot` and merges new and changed strings into each
   catalog, marking changed ones `fuzzy`.
2. Edit `po/<lang>.po` with a PO editor (Poedit, Lokalize, GNOME Translation Editor) or a text
   editor. Translate the empty entries and review every `fuzzy` one, then remove its `fuzzy` flag.
3. Check the catalog with `msgfmt --check --statistics -o /dev/null po/<lang>.po`.
4. Try it from the source tree:

   ```bash
   po/build.sh target/locale
   PDFS_LOCALEDIR=target/locale LANGUAGE=<lang> cargo run -p pdfs-gui --bin pdfs-app
   ```

## Adding a language

1. Add the code (for example `sv` or `pt_PT`) to `po/LINGUAS`, keeping the list sorted.
2. Run `po/update.sh`. It creates `po/<lang>.po` from the template with `msginit`.
3. Fill in the `Plural-Forms` header if `msginit` did not, and translate the catalog.
4. Add the code and the language's own name to `LANGUAGES` in `crates/pdfs-gui/src/i18n.rs`, so the
   Preferences picker offers it.
5. Add `GenericName[<lang>]`, `Comment[<lang>]` and `Keywords[<lang>]` lines to the two desktop files
   in `packaging/`.

`cargo test -p pdfs-gui` checks that every language in the picker has a catalog, that
`po/POTFILES.in` lists every source file that calls gettext, and that every catalog passes
`msgfmt --check`.

## Rules for translators

- Keep every `{placeholder}` from the English text. You may move it, but not rename or drop it.
  `msgfmt --check` rejects a catalog where they differ. Plural forms keep `{n}` in every form.
- Read the `#.` translator comments above an entry. They explain placeholders and date formats.
- Date entries such as `%b %e` are `strftime` formats for GLib's `DateTime::format`. Translate them
  into the order and separators your language uses, not into literal dates.
- A `msgctxt` such as `verb` or `column` tells you which meaning of a short word is meant.
- Keep product names as they are: Proton Drive, Proton, Google Takeout.
- Use the same term for the same thing across the catalog, and follow the terminology of your
  desktop (GNOME or KDE) for common actions such as Trash, Preferences and Cancel.

## Rules for developers

- Wrap every user-visible GUI string: `gettext("Move to Trash")`.
- Never build a sentence from translated pieces or with `format!`. Use named placeholders:
  `gettext_f("Moved {name} to Trash", &[("name", &name)])`.
- Counts use `ngettext_f("{n} item", "{n} items", count, &[])`; `{n}` is filled in for you.
- Give short, ambiguous words a context: `pgettext("verb", "Open")`.
- Mark labels in `const` tables with `gettext_noop` and translate them where they are shown.
- The message id must be a single string literal on one line, since `xgettext` reads literals only.
- Add a `// Translators:` comment on the line above a call whose meaning is not obvious.
- Do not translate log messages, CLI output, action or icon names, or values the daemon sends back.
- Run `po/update.sh` after changing strings and commit the updated template and catalogs.
