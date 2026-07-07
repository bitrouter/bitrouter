#!/usr/bin/env python3
"""Regenerate the models/providers tables in docs/get-started/supported-*.

Source of truth is the generated registry catalog dist/registry/{models,providers}.json
(rebuild it with `cargo run -p dist-helper -- registry build` after editing
registry/). This script rewrites the table under a fixed anchor heading in each
doc, so the surrounding prose is preserved. English and Chinese pages share the
same data rows (only the header row differs), keeping the translations in lockstep.

Usage: python3 scripts/gen-supported-tables.py
"""
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MODELS = json.loads((ROOT / "dist/registry/models.json").read_text())["data"]
PROVIDERS = json.loads((ROOT / "dist/registry/providers.json").read_text())["data"]


def cell(s):
    return str(s).replace("|", r"\|")


# Common context windows use mixed decimal/binary conventions; map the known
# sizes to their conventional label, and fall back to decimal K/M otherwise.
CTX_SIZES = {
    128000: "128K", 131072: "128K", 196608: "192K", 200000: "200K",
    202752: "198K", 256000: "256K", 262144: "256K", 524288: "512K",
    1000000: "1M", 1048576: "1M", 2000000: "2M", 2097152: "2M", 4194304: "4M",
}


def ctx(n):
    if not n:
        return "—"
    if n in CTX_SIZES:
        return CTX_SIZES[n]
    if n >= 1_000_000:
        return f"{n / 1_000_000:g}M"
    return f"{round(n / 1000)}K"


def usd(v):
    return "—" if v is None else f"${v:g}"


def best_price(model):
    """Cheapest listed input price across serving providers, with that provider's output price."""
    best = None
    for pr in model.get("providers", []):
        pg = pr.get("pricing")
        if not pg:
            continue
        i = (pg.get("input_tokens") or {}).get("no_cache")
        if i is None:
            continue
        o = (pg.get("output_tokens") or {}).get("text")
        if best is None or i < best[0]:
            best = (i, o)
    return best or (None, None)


def model_rows():
    rows = []
    for m in sorted(MODELS, key=lambda x: x["id"]):
        i, o = best_price(m)
        rows.append(
            "| {id} | {name} | {ctx} | {mod} | {open} | {i} | {o} |".format(
                id=f"`{m['id']}`",
                name=cell(m.get("name", m["id"])),
                ctx=ctx(m.get("max_input_tokens")),
                mod=cell(", ".join(m.get("input_modalities", [])) or "—"),
                open="✅" if m.get("open_weights") else "—",
                i=usd(i),
                o=usd(o),
            )
        )
    return rows


BILLING = {"usage_token": "Per-token", "subscription": "Subscription"}


def provider_rows():
    rows = []
    for p in sorted(PROVIDERS, key=lambda x: x["id"]):
        meta = p.get("metadata") or {}
        protos = set()
        for m in p.get("models", []):
            ap = m.get("api_protocol")
            if isinstance(ap, list):
                protos.update(ap)
            elif ap:
                protos.add(ap)
        protocols = sorted(protos)
        rows.append(
            "| {id} | {name} | {hq} | {proto} | {billing} | {n} |".format(
                id=f"`{p['id']}`",
                name=cell(meta.get("name", p["id"])),
                hq=cell(meta.get("headquarters", "—")),
                proto=cell(", ".join(protocols) or "—"),
                billing=BILLING.get(p.get("billing"), cell(p.get("billing", "—"))),
                n=len(p.get("models", [])),
            )
        )
    return rows


def table(header, rows):
    cols = header.count("|") - 1
    return "\n".join([header, "| " + " | ".join(["---"] * cols) + " |", *rows])


# (file, anchor heading, header row, rows)
MODELS_HDR_EN = "| Model | Name | Context | Modalities | Open weights | Input $/M | Output $/M |"
MODELS_HDR_ZH = "| 模型 | 名称 | 上下文 | 模态 | 开源权重 | 输入 $/M | 输出 $/M |"
PROV_HDR_EN = "| Provider | Name | HQ | Protocols | Billing | Models |"
PROV_HDR_ZH = "| 供应商 | 名称 | 总部 | 协议 | 计费 | 模型数 |"

TARGETS = [
    ("docs/get-started/supported-models.md", "## Model catalog", MODELS_HDR_EN, model_rows),
    ("docs/get-started/supported-models.zh.md", "## 模型目录", MODELS_HDR_ZH, model_rows),
    ("docs/get-started/supported-providers.md", "## Provider directory", PROV_HDR_EN, provider_rows),
    ("docs/get-started/supported-providers.zh.md", "## 供应商目录", PROV_HDR_ZH, provider_rows),
]


def rewrite(path, anchor, header, rows):
    p = ROOT / path
    lines = p.read_text().splitlines()
    try:
        start = next(i for i, ln in enumerate(lines) if ln.strip() == anchor)
    except StopIteration:
        raise SystemExit(f"anchor {anchor!r} not found in {path}")
    end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")), len(lines))
    block = ["", table(header, rows), ""]
    new = lines[: start + 1] + block + lines[end:]
    p.write_text("\n".join(new) + "\n")


def main():
    mr, pr = model_rows(), provider_rows()
    for path, anchor, header, builder in TARGETS:
        rewrite(path, anchor, header, builder())
    print(f"gen-supported-tables: wrote {len(mr)} models, {len(pr)} providers into 4 docs")


if __name__ == "__main__":
    main()
