#!/usr/bin/env python3
"""List untranslated Spanish msgids in the recording feature; silence is success."""

import ast
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SOURCES = (
    "src/app.rs", "src/app/calls.rs", "src/model.rs", "src/settings.rs",
    "src/call_capture.rs", "src/call_tape.rs", "src/ui/call.rs",
    "src/ui/call_review.rs", "src/ui/calls.rs", "src/ui/conversation.rs",
    "src/ui/projects.rs", "src/ui/leads.rs", "src/ui/settings.rs",
    "src/ui/take.rs",
)


def entries(path):
    """Read PO fields, including continued strings and plural forms."""
    entry, field, fuzzy = {}, None, False
    for line in path.read_text().splitlines() + [""]:
        if entry.get("msgid") and field and field.startswith("msgstr") and not line.startswith(('"', "msgstr[")):
            entry["fuzzy"] = fuzzy
            yield entry
            entry, field, fuzzy = {}, None, False
        if not line.strip() and not entry:
            fuzzy = False
        if line.startswith("#, ") and "fuzzy" in line:
            fuzzy = True
        elif line.startswith('"') and field:
            entry[field] += ast.literal_eval(line)
        elif line.startswith(("msgid ", "msgid_plural ", "msgctxt ", "msgstr ", "msgstr[")):
            field, value = line.split(" ", 1)
            entry[field] = ast.literal_eval(value)


def main():
    cache = ROOT / ".cache"
    cache.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="recording-i18n-", dir=cache) as scratch:
        template = Path(scratch) / "messages.pot"
        subprocess.run([
            "xgettext", "--language=Rust", "--from-code=UTF-8",
            "--keyword=gettext:2", "--keyword=ngettext:2,3", "--keyword=pgettext:2c,3",
            "--keyword=translated:2", f"--output={template}",
            *(source for source in SOURCES if (ROOT / source).is_file()),
        ], cwd=ROOT, check=True)
        translated = {
            (entry.get("msgctxt"), entry["msgid"])
            for entry in entries(ROOT / "assets/i18n/es.po")
            if not entry["fuzzy"] and (
                entry.get("msgstr") or (entry.get("msgstr[0]") and entry.get("msgstr[1]"))
            )
        }
        missing = sorted({
            entry["msgid"] for entry in entries(template)
            if (entry.get("msgctxt"), entry["msgid"]) not in translated
        })
        for msgid in missing:
            print(msgid)
        return bool(missing)


if __name__ == "__main__":
    raise SystemExit(main())
