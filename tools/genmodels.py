"""Generate src/models.json (the embedded catalog fallback) from catalog-fixture.json.

Run from anywhere: python3 -I tools/genmodels.py
Paths are relative to this file; the script adds no dependencies beyond the stdlib.
"""

import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
SRC = HERE / "catalog-fixture.json"
OUT = HERE.parent / "src" / "models.json"
ALIAS = "gpt-5.6"
ALIAS_SOURCE = "gpt-5.6-sol"

src = json.loads(SRC.read_text(encoding="utf-8"))
models = [m for m in src["models"] if not m.get("is_disabled")]
sol = next((m for m in models if m["id"] == ALIAS_SOURCE), None)
alias = sol is not None and not any(m["id"] == ALIAS for m in models)


def limit(m, key):
    """C3 fields only carry positive integers; everything else is omitted."""
    value = m.get(key)
    return value if isinstance(value, int) and not isinstance(value, bool) and value > 0 else None


def entry(slug, m, priority):
    levels = m.get("supported_effort_levels") or []
    name = m.get("display_name", slug)
    e = {"slug": slug, "display_name": name, "description": "Zed - " + name}
    d = next((l for l in levels if l.get("is_default")), None)
    if d:
        e["default_reasoning_level"] = d["value"].lower()
    e["supported_reasoning_levels"] = [
        {"effort": l["value"].lower(), "description": l.get("name", l["value"])}
        for l in levels
    ]
    e.update({"shell_type": "shell_command", "visibility": "list", "supported_in_api": True, "priority": priority})
    return e


def data_entry(slug, m):
    item = {"id": slug, "object": "model", "owned_by": m.get("provider", "anthropic")}
    context = limit(m, "max_token_count")
    if context is not None:
        item["context_length"] = context
    output = limit(m, "max_output_tokens")
    if output is not None:
        item["max_output_tokens"] = output
        item["max_completion_tokens"] = output
    return item


data, cat = [], []
if alias:
    data.append(data_entry(ALIAS, sol))
for m in models:
    data.append(data_entry(m["id"], m))
pri = 1
if alias:
    cat.append(entry(ALIAS, sol, pri)); pri += 1
for m in models:
    cat.append(entry(m["id"], m, pri)); pri += 1
out = {"object": "list", "data": data, "models": cat}
with open(OUT, "w", encoding="utf-8", newline="\n") as f:
    f.write(json.dumps(out, indent=2, ensure_ascii=False) + "\n")
print(f"wrote {OUT}: {len(data)} data, {len(cat)} models")
