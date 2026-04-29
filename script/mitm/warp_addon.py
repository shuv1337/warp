"""mitmproxy addon for capturing app.warp.dev traffic.

Loaded via:

    mitmdump -s script/mitm/warp_addon.py \
        --listen-port 8080 \
        --set output_dir=captures

Behavior:

  * Scope filter: only `app.warp.dev` and `*.app.warp.dev` (and optionally
    `rtc.app.warp.dev` for future Stage D investigation) are captured. All
    other flows pass through untouched and are not written to disk by this
    addon.

  * Per-flow artifacts under `output_dir` (default `captures/`):

        <ts>__<safe-method-path>__meta.json   # redacted headers + summary
        <ts>__<safe-method-path>__request.bin
        <ts>__<safe-method-path>__response.bin

    For SSE responses to `/ai/multi-agent`, a third file is written:

        <ts>__<safe-method-path>__response.sse

    which preserves event boundaries. Each `data:` payload is then decoded
    (URL-safe-base64 → protobuf bytes) into:

        <ts>__<safe-method-path>__events.decoded.json

    ...if the matching `warp_multi_agent_api` Python protobuf modules are
    importable. Otherwise the decoded.json file contains the raw decoded
    bytes (hex) so you can finish decoding offline.

  * Redaction in the committed metadata view (`*.meta.json`):

        Authorization, Cookie, Set-Cookie, Proxy-Authorization
        x-api-key, x-warp-key, anything starting with `x-auth-`

    are replaced with `<redacted>`. Raw bodies are NEVER copied into
    meta.json; meta.json only carries headers, status, timing, and
    URL/scope info.

  * The addon is conservative: any error in body decoding or proto parsing
    is logged but never aborts the capture.

Note that even the `*.bin` and `*.sse` artifacts are sensitive — they
contain prompts, file contents, and provider keys in request headers. The
repo's `.gitignore` and `script/git/check-captures.sh` guard refuse to
let those files into git. Only hand-scrubbed Markdown summaries under
`captures/redacted/` may be committed.
"""

from __future__ import annotations

import base64
import binascii
import json
import logging
import os
import re
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable

from mitmproxy import ctx, http
from mitmproxy.addonmanager import Loader

# Hosts we scope to. Anything else is a complete no-op for this addon.
_WARP_HOST_RE = re.compile(r"(?:^|\.)app\.warp\.dev$", re.IGNORECASE)
_RTC_HOST_RE = re.compile(r"^rtc\.app\.warp\.dev$", re.IGNORECASE)

# Header names that must always be redacted in committed metadata.
_REDACT_HEADERS = {
    "authorization",
    "cookie",
    "set-cookie",
    "proxy-authorization",
    "x-api-key",
    "x-warp-key",
}
_REDACT_HEADER_PREFIXES = ("x-auth-",)

# Try to import the Python protobuf bindings for `warp_multi_agent_api`.
# The repo pins `rev = "78a78f21..."` of warp-proto-apis (see root
# `Cargo.toml`). If the user has the corresponding Python modules on
# PYTHONPATH the addon will use them; otherwise we fall back to writing
# raw decoded bytes (hex) for offline analysis.
try:  # pragma: no cover — depends on user environment
    from warp_multi_agent_api import response_event_pb2  # type: ignore

    _HAVE_PROTO = True
except Exception:  # noqa: BLE001
    response_event_pb2 = None
    _HAVE_PROTO = False


@dataclass
class _Settings:
    output_dir: Path = field(default_factory=lambda: Path("captures"))
    include_rtc: bool = False


_settings = _Settings()


def load(loader: Loader) -> None:
    loader.add_option(
        name="output_dir",
        typespec=str,
        default="captures",
        help="Directory to write captured Warp flows into.",
    )
    loader.add_option(
        name="include_rtc",
        typespec=bool,
        default=False,
        help="Also capture rtc.app.warp.dev (Stage D investigation).",
    )


def configure(updates: Iterable[str]) -> None:
    if "output_dir" in updates:
        _settings.output_dir = Path(ctx.options.output_dir)
    if "include_rtc" in updates:
        _settings.include_rtc = bool(ctx.options.include_rtc)
    _settings.output_dir.mkdir(parents=True, exist_ok=True)
    if not _HAVE_PROTO:
        ctx.log.info(
            "warp_addon: warp_multi_agent_api proto modules not available; "
            "SSE bodies will be saved raw and `.events.decoded.json` will "
            "contain hex-encoded payloads."
        )


def _is_warp_host(host: str) -> bool:
    if not host:
        return False
    if _WARP_HOST_RE.search(host):
        return True
    if _settings.include_rtc and _RTC_HOST_RE.match(host):
        return True
    return False


def _safe_path_component(s: str) -> str:
    """Filesystem-safe slug. Must round-trip nothing — just be readable."""
    s = s.strip("/")
    s = re.sub(r"[^A-Za-z0-9._-]+", "-", s)
    s = s.strip("-")
    return s[:120] if s else "root"


def _redact_headers(headers: http.Headers) -> dict[str, list[str]]:
    out: dict[str, list[str]] = {}
    for name, value in headers.items(multi=True):
        lname = name.lower()
        if lname in _REDACT_HEADERS or any(
            lname.startswith(p) for p in _REDACT_HEADER_PREFIXES
        ):
            out.setdefault(name, []).append("<redacted>")
        else:
            out.setdefault(name, []).append(value)
    return out


def _flow_basename(flow: http.HTTPFlow) -> str:
    ts = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(flow.timestamp_start or time.time()))
    method = flow.request.method.upper()
    path = _safe_path_component(flow.request.path or "/")
    # Add a millisecond suffix to keep simultaneous requests distinct.
    ms = int((flow.timestamp_start or time.time()) * 1000) % 1000
    return f"{ts}-{ms:03d}__{method}__{path}"


def _artifact_path(base: Path, suffix: str) -> Path:
    """Return ``<base><suffix>`` as a Path.

    `Path.with_suffix` only replaces the final suffix, so it can't be used
    with compound suffixes like ``.events.decoded.json``. We just append.
    """
    if not suffix.startswith("."):
        suffix = "." + suffix
    return base.with_name(base.name + suffix)


def _is_multi_agent_sse(flow: http.HTTPFlow) -> bool:
    if flow.response is None:
        return False
    if flow.request.path != "/ai/multi-agent":
        return False
    ctype = (flow.response.headers.get("content-type") or "").lower()
    # Could be `text/event-stream` or just streamed plain text. Treat any
    # /ai/multi-agent response as SSE-shaped for capture purposes.
    return ctype.startswith("text/event-stream") or "/ai/multi-agent" == flow.request.path


def _decode_multi_agent_sse(raw: bytes) -> list[dict]:
    """Decode an SSE body into a list of records.

    Each record looks like:

        {
          "event": "<event name or empty>",
          "data_b64": "<original data: line, with quotes stripped>",
          "decoded_hex": "<bytes after base64-decoding, as hex>",
          "proto":      <parsed ResponseEvent as a dict, if proto support
                         is available>
          "decode_error": "<reason>"   # only if decoding failed
        }
    """
    records: list[dict] = []
    if not raw:
        return records

    text = raw.decode("utf-8", errors="replace")

    # SSE: events are separated by blank lines; each event is a sequence of
    # `field: value` lines. We only care about `event:` and `data:`.
    for chunk in re.split(r"\r?\n\r?\n", text):
        chunk = chunk.strip()
        if not chunk:
            continue
        event_name = ""
        data_lines: list[str] = []
        for line in chunk.splitlines():
            if line.startswith(":"):
                continue  # SSE comment
            if line.startswith("event:"):
                event_name = line[len("event:") :].strip()
            elif line.startswith("data:"):
                data_lines.append(line[len("data:") :].lstrip())
        if not data_lines:
            continue
        data_payload = "\n".join(data_lines)

        record: dict = {"event": event_name, "data_b64": data_payload}

        # Trim any wrapping quotes that the server uses for JSON-string framing
        # of base64 payloads.
        candidate = data_payload
        if len(candidate) >= 2 and candidate[0] == candidate[-1] and candidate[0] in ('"', "'"):
            candidate = candidate[1:-1]

        try:
            decoded = base64.urlsafe_b64decode(candidate + "=" * (-len(candidate) % 4))
            record["decoded_hex"] = binascii.hexlify(decoded).decode("ascii")
            if _HAVE_PROTO:
                try:
                    msg = response_event_pb2.ResponseEvent()  # type: ignore[union-attr]
                    msg.ParseFromString(decoded)
                    # `MessageToDict` from google.protobuf would be ideal but
                    # we keep the dependency surface minimal; instead emit a
                    # text form via str(msg) and let the human read it.
                    record["proto_text"] = str(msg)
                except Exception as exc:  # noqa: BLE001
                    record["proto_decode_error"] = repr(exc)
        except Exception as exc:  # noqa: BLE001
            record["decode_error"] = repr(exc)

        records.append(record)
    return records


def response(flow: http.HTTPFlow) -> None:
    try:
        if not _is_warp_host(flow.request.pretty_host):
            return
        if flow.response is None:
            return

        base = _settings.output_dir / _flow_basename(flow)
        base.parent.mkdir(parents=True, exist_ok=True)

        # Raw bodies (gitignored — local-only).
        req_bytes = flow.request.raw_content or b""
        resp_bytes = flow.response.raw_content or b""
        _artifact_path(base, ".request.bin").write_bytes(req_bytes)
        _artifact_path(base, ".response.bin").write_bytes(resp_bytes)

        # SSE-aware decode for /ai/multi-agent.
        if _is_multi_agent_sse(flow):
            _artifact_path(base, ".response.sse").write_bytes(resp_bytes)
            try:
                events = _decode_multi_agent_sse(resp_bytes)
                _artifact_path(base, ".events.decoded.json").write_text(
                    json.dumps(
                        {
                            "have_proto": _HAVE_PROTO,
                            "event_count": len(events),
                            "events": events,
                        },
                        indent=2,
                    ),
                    encoding="utf-8",
                )
            except Exception as exc:  # noqa: BLE001
                logging.exception("warp_addon: SSE decode failed: %s", exc)

        # Redacted metadata (still LOCAL — guard refuses to commit anything
        # under captures/ except the redacted/ allowlist).
        meta = {
            "ts_start": flow.timestamp_start,
            "ts_end": flow.timestamp_end,
            "method": flow.request.method,
            "scheme": flow.request.scheme,
            "host": flow.request.pretty_host,
            "path": flow.request.path,
            "request_headers": _redact_headers(flow.request.headers),
            "response_status": flow.response.status_code,
            "response_headers": _redact_headers(flow.response.headers),
            "request_size": len(req_bytes),
            "response_size": len(resp_bytes),
            "warp_proto_decoder_available": _HAVE_PROTO,
        }
        _artifact_path(base, ".meta.json").write_text(
            json.dumps(meta, indent=2, sort_keys=True),
            encoding="utf-8",
        )
    except Exception as exc:  # noqa: BLE001
        # Never let an addon error abort the capture.
        logging.exception("warp_addon: error handling flow: %s", exc)
