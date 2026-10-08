# AI Usage Tray: reads Codex and Claude Code token usage on this machine and
# prints it summed by half hour (UTC) and model. Sent over SSH and run with
# `python3 -`; nothing is installed. Follows the same rules as the app's own
# reader (crates/core/src/cost/scan.rs). A small state file under
# ~/.cache/usage-monitor lets the next run read only what the logs added.
# The app puts `KNOWN = "<token>"` before the script: the token of the last
# reply it kept. When it matches the one this machine last sent, only the
# rows that changed since are sent.
import json
import os
import sys
from datetime import datetime

VERSION = 2
# Bumped when the state file's shape or what it keeps changes.
CACHE_VERSION = 2
SLOT_SECONDS = 1800
CODEX_LONG_CONTEXT_TOKENS = 272000
CODEX_PREFIX_BYTES = 192

home = os.path.expanduser("~")
codex_home = os.environ.get("CODEX_HOME") or os.path.join(home, ".codex")
codex_roots = [os.path.join(codex_home, "sessions"), os.path.join(codex_home, "archived_sessions")]
claude_config = os.environ.get("CLAUDE_CONFIG_DIR", "").strip()
if claude_config:
    claude_roots = [
        os.path.join(part.strip(), "projects")
        for part in claude_config.replace(";", ",").split(",")
        if part.strip()
    ]
else:
    claude_roots = [
        os.path.join(home, ".claude", "projects"),
        os.path.join(home, ".config", "claude", "projects"),
    ]
cache_dir = os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.join(home, ".cache"), "usage-monitor")
cache_path = os.path.join(cache_dir, "remote-scan.json")
# The token and rows of the last reply.
sent_path = os.path.join(cache_dir, "remote-sent.json")
known = globals().get("KNOWN", "")


def slot_of(timestamp):
    if not isinstance(timestamp, str) or not timestamp:
        return None
    try:
        text = timestamp.replace("Z", "+00:00")
        # Python before 3.11 takes at most six fraction digits.
        if "." in text:
            head, rest = text.split(".", 1)
            digits = ""
            while rest and rest[0].isdigit():
                digits += rest[0]
                rest = rest[1:]
            text = head + "." + (digits[:6].ljust(6, "0")) + rest
        seconds = datetime.fromisoformat(text).timestamp()
    except (ValueError, OverflowError):
        return None
    return int(seconds // SLOT_SECONDS * SLOT_SECONDS)


def number(value):
    return value if isinstance(value, int) and value > 0 else 0


def jsonl_files(root):
    found = []
    for directory, _, names in os.walk(root):
        for name in names:
            if name.endswith(".jsonl"):
                found.append(os.path.join(directory, name))
    return found


def complete_lines(handle):
    """Yields each complete line and the bytes read through it."""
    consumed = 0
    while True:
        line = handle.readline()
        if not line or not line.endswith(b"\n"):
            return
        consumed += len(line)
        yield line, consumed


def read_codex(handle, entry):
    rows = entry["rows"]
    consumed = 0
    for line, consumed in complete_lines(handle):
        prefix = line[:CODEX_PREFIX_BYTES]
        if b'"type":"token_count"' not in prefix and b'"type":"turn_context"' not in prefix:
            continue
        try:
            parsed = json.loads(line)
        except ValueError:
            continue
        payload = parsed.get("payload") if isinstance(parsed, dict) else None
        if not isinstance(payload, dict):
            continue
        if parsed.get("type") == "turn_context":
            model = payload.get("model")
            if isinstance(model, str) and model.strip():
                entry["model"] = model
            continue
        if payload.get("type") != "token_count":
            continue
        info = payload.get("info")
        if not isinstance(info, dict):
            continue
        total_usage = info.get("total_token_usage")
        total = None
        if isinstance(total_usage, dict):
            total = number(total_usage.get("total_tokens")) or (
                number(total_usage.get("input_tokens")) + number(total_usage.get("output_tokens"))
            )
        if total is not None and total == entry["last_total"]:
            continue
        if total is not None:
            entry["last_total"] = total
        last = info.get("last_token_usage")
        if not isinstance(last, dict):
            continue
        model = info.get("model") or payload.get("model")
        if not isinstance(model, str) or not model.strip():
            model = entry["model"] or "unknown"
        slot = slot_of(parsed.get("timestamp"))
        if slot is None:
            continue
        input_tokens = number(last.get("input_tokens"))
        cache_read = min(number(last.get("cached_input_tokens")), input_tokens)
        cache_write = min(number(last.get("cache_write_input_tokens")), input_tokens - cache_read)
        output = number(last.get("output_tokens"))
        tokens = [
            input_tokens - cache_read - cache_write,
            cache_read,
            cache_write,
            0,
            output,
            min(number(last.get("reasoning_output_tokens")), output),
        ]
        if tokens[0] + tokens[1] + tokens[2] + tokens[4] == 0:
            continue
        key = "%d|%s|%d" % (slot, model, 1 if input_tokens > CODEX_LONG_CONTEXT_TOKENS else 0)
        row = rows.setdefault(key, [0, 0, 0, 0, 0, 0])
        for index, value in enumerate(tokens):
            row[index] += value
    return consumed


def read_claude(handle, entry):
    records = entry["claude"]
    consumed = 0
    for line, consumed in complete_lines(handle):
        if b'"usage"' not in line:
            continue
        try:
            parsed = json.loads(line)
        except ValueError:
            continue
        if not isinstance(parsed, dict):
            continue
        message = parsed.get("message")
        if not isinstance(message, dict):
            continue
        usage = message.get("usage")
        model = message.get("model")
        if not isinstance(usage, dict) or not isinstance(model, str):
            continue
        if not model.strip() or model.strip() == "<synthetic>":
            continue
        slot = slot_of(parsed.get("timestamp"))
        if slot is None:
            continue
        cache_write = number(usage.get("cache_creation_input_tokens"))
        creation = usage.get("cache_creation")
        hour = number(creation.get("ephemeral_1h_input_tokens")) if isinstance(creation, dict) else 0
        tokens = [
            number(usage.get("input_tokens")),
            number(usage.get("cache_read_input_tokens")),
            cache_write,
            min(hour, cache_write),
            number(usage.get("output_tokens")),
            0,
        ]
        if tokens[0] + tokens[1] + tokens[2] + tokens[4] == 0:
            continue
        message_id = message.get("id")
        request_id = parsed.get("requestId")
        if message_id and request_id:
            key = "%s:%s" % (message_id, request_id)
        elif message_id:
            key = str(message_id)
        else:
            key = "%s:%d" % (parsed.get("timestamp") or "", len(line) - 1)
        records.append([key, slot, model] + tokens)
    return consumed


def new_entry(tool):
    return {"tool": tool, "size": 0, "mtime": 0, "offset": 0, "model": None,
            "last_total": None, "rows": {}, "claude": []}


def main():
    try:
        with open(cache_path, "r", encoding="utf-8") as handle:
            cache = json.load(handle)
        if cache.get("version") != CACHE_VERSION:
            cache = {}
    except (OSError, ValueError):
        cache = {}
    previous_files = cache.get("files", {}) if isinstance(cache, dict) else {}

    files = {}
    found = []
    for tool, roots in (("codex", codex_roots), ("claude", claude_roots)):
        for root in roots:
            if not os.path.isdir(root):
                continue
            if tool not in found:
                found.append(tool)
            for path in sorted(jsonl_files(root)):
                try:
                    status = os.stat(path)
                except OSError:
                    continue
                mtime = int(status.st_mtime)
                entry = previous_files.get(path)
                if entry and entry.get("tool") == tool and entry["size"] == status.st_size and entry["mtime"] == mtime:
                    files[path] = entry
                    continue
                if not entry or entry.get("tool") != tool or entry["offset"] > status.st_size:
                    entry = new_entry(tool)
                try:
                    with open(path, "rb") as handle:
                        handle.seek(entry["offset"])
                        reader = read_codex if tool == "codex" else read_claude
                        entry["offset"] += reader(handle, entry)
                except OSError:
                    continue
                entry["size"] = status.st_size
                entry["mtime"] = mtime
                files[path] = entry

    try:
        os.makedirs(cache_dir, exist_ok=True)
        temporary = cache_path + ".tmp"
        with open(temporary, "w", encoding="utf-8") as handle:
            json.dump({"version": CACHE_VERSION, "files": files}, handle)
        os.replace(temporary, cache_path)
    except OSError:
        pass

    totals = {}
    responses = {}
    for entry in files.values():
        for key, row in entry["rows"].items():
            total = totals.setdefault("codex|" + key, [0, 0, 0, 0, 0, 0])
            for index, value in enumerate(row):
                total[index] += value
        for record in entry["claude"]:
            kept = responses.get(record[0])
            # A response written twice keeps its fuller usage.
            if kept is None or record[7] > kept[7]:
                responses[record[0]] = record
    for record in responses.values():
        total = totals.setdefault("claude|%d|%s|0" % (record[1], record[2]), [0, 0, 0, 0, 0, 0])
        for index, value in enumerate(record[3:9]):
            total[index] += value

    try:
        with open(sent_path, "r", encoding="utf-8") as handle:
            sent = json.load(handle)
        if not isinstance(sent, dict):
            sent = {}
    except (OSError, ValueError):
        sent = {}
    sent_rows = sent.get("rows", {}) if isinstance(sent.get("rows"), dict) else {}
    partial = bool(known) and known == sent.get("token")
    token = os.urandom(8).hex()

    changed = {key: tokens for key, tokens in totals.items() if not partial or sent_rows.get(key) != tokens}
    if partial:
        # A row that is gone is sent as zero, which the app drops.
        for key in sent_rows:
            if key not in totals:
                changed[key] = [0, 0, 0, 0, 0, 0]
    try:
        os.makedirs(cache_dir, exist_ok=True)
        temporary = sent_path + ".tmp"
        with open(temporary, "w", encoding="utf-8") as handle:
            json.dump({"token": token, "rows": totals}, handle)
        os.replace(temporary, sent_path)
    except OSError:
        # Without a record of what was sent, the next reply must be whole.
        token = ""

    rows = []
    for key, tokens in changed.items():
        tool, slot, rest = key.split("|", 2)
        model, long_context = rest.rsplit("|", 1)
        rows.append([tool, int(slot), model, int(long_context)] + tokens)
    reply = {"usage_monitor": VERSION, "found": found, "rows": rows, "token": token, "partial": partial}
    sys.stdout.write(json.dumps(reply, ensure_ascii=True))
    sys.stdout.write("\n")


main()
