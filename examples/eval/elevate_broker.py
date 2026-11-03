#!/usr/bin/env python3
"""Native-only, loopback credential boundary for the production Elevate evaluator.

Run via pass-env with the dedicated SPEECH_EVAL entry, never on Boat. Reverse
forward this loopback port with `boat forward ID --reverse --local PORT --remote
PORT --bind 127.0.0.1`. Every upstream body originates in postproc::refine; only
max_completion_tokens and provider price/fallback guards are added here.

--inspect reads the live counter/catalog without inference. --out must be fresh,
private scratch outside the repo. Only accounting.json is publishable; raw-*.json
contain model outputs and must remain private. No headers, keys or transcript
text are printed. A refused request, unknown cost, interrupted request or missing
post-call counter halts the broker; a new invocation is not a paid retry permit.
"""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import sys
import time
import urllib.error
import urllib.request
from decimal import Decimal

MODEL = "openai/gpt-6.1-sol"
CEILING = Decimal("4.50")
COMPLETION_CAP = 1024
RESPONSE_LIMIT = 2_000_000


def digest(value):
    return hashlib.sha256(value).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=4949)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--inspect", action="store_true")
    args = parser.parse_args()
    key = os.environ.pop("OPENROUTER_API_KEY", "").removesuffix("\n")
    if not key or not key.isascii() or not all(33 <= ord(c) <= 126 for c in key):
        raise RuntimeError("eval_credential_invalid")
    if not args.out.is_absolute() or args.out.exists():
        raise RuntimeError("fresh_absolute_private_out_required")
    # A credential process may not create public model-output artifacts in its checkout.
    if args.out.resolve().is_relative_to(Path.cwd().resolve()):
        raise RuntimeError("private_out_must_be_outside_repository")
    args.out.mkdir(mode=0o700)
    os.umask(0o077)

    def api(path, body=None):
        request = urllib.request.Request(
            "https://openrouter.ai/api/v1/" + path,
            data=body,
            headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"},
            method="POST" if body is not None else "GET",
        )
        with urllib.request.urlopen(request, timeout=25) as response:
            data = response.read(RESPONSE_LIMIT + 1)
            if len(data) > RESPONSE_LIMIT:
                raise RuntimeError("upstream_response_limit")
            return json.loads(data), data

    def usage():
        value, _ = api("key")
        amount = Decimal(str(value["data"]["usage"]))
        if not amount.is_finite() or amount < 0:
            raise RuntimeError("key_counter_invalid")
        return amount

    start = usage()
    catalog, _ = api("models")
    model = next(row for row in catalog["data"] if row["id"] == MODEL)
    prompt_price = Decimal(model["pricing"]["prompt"])
    completion_price = Decimal(model["pricing"]["completion"])
    request_price = Decimal(model["pricing"].get("request", "0"))
    if not all(value.is_finite() and value >= 0 for value in (prompt_price, completion_price, request_price)):
        raise RuntimeError("model_price_invalid")
    accounting = {
        "schema": "cantrip.eval.native-broker.v1",
        "model": MODEL,
        "canonical_model": model.get("canonical_slug"),
        "reasoning_effort": "low",
        "absolute_key_ceiling_usd": float(CEILING),
        "key_usage_start_usd": float(start),
        "key_usage_end_usd": float(start),
        "key_usage_delta_usd": 0.0,
        "test_only_max_completion_tokens": COMPLETION_CAP,
        "prompt_usd_per_token": float(prompt_price),
        "completion_usd_per_token": float(completion_price),
        "request_usd": float(request_price),
        "provider_guards": {"allow_fallbacks": False, "require_parameters": True,
                            "max_price": {"prompt": float(prompt_price * 1_000_000),
                                          "completion": float(completion_price * 1_000_000)}},
        "counter_policy": "Read before and after every serial call; reserve byte-bounded input plus 256 framing tokens and 1024 completion tokens at capped prices, with a 10% margin. Unknown liability halts; no paid retries.",
        "credential_placement": "Native pass-env process only; reverse-forwarded loopback HTTP carries no authorization header from the worker.",
        "calls": [],
        "halted": start >= CEILING,
    }
    reported_total = Decimal(0)

    def persist():
        with (args.out / "accounting.json").open("w") as file:
            json.dump(accounting, file, indent=2)
            file.flush()
            os.fsync(file.fileno())

    persist()
    if args.inspect:
        print(json.dumps(accounting, sort_keys=True))
        return

    class Handler(http.server.BaseHTTPRequestHandler):
        # BaseHTTPRequestHandler otherwise prints URLs, request lines and client data.
        def log_message(self, *_):
            pass

        def reply(self, status, value):
            data = json.dumps(value, separators=(",", ":")).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            try:
                self.wfile.write(data)
            except (BrokenPipeError, ConnectionResetError):
                accounting["halted"] = True
                persist()

        def do_GET(self):
            if self.path != "/api/v1/accounting":
                self.reply(404, {"error": "route_not_allowed"})
                return
            self.reply(200, accounting)

        def do_POST(self):
            nonlocal reported_total
            if self.path != "/api/v1/chat/completions" or accounting["halted"]:
                self.reply(403, {"error": "broker_halted_or_route_not_allowed"})
                return
            record = None
            try:
                if self.headers.get("Authorization") is not None or self.headers.get("Transfer-Encoding") is not None:
                    raise RuntimeError("worker_auth_or_chunking_not_allowed")
                size = int(self.headers.get("Content-Length", "0"))
                if not 0 < size <= 30_000:
                    raise RuntimeError("production_request_size_invalid")
                original = self.rfile.read(size)
                body = json.loads(original)
                if (set(body) != {"model", "messages", "reasoning"}
                        or body["model"] != MODEL or body["reasoning"] != {"effort": "low"}
                        or len(body["messages"]) != 2
                        or [message["role"] for message in body["messages"]] != ["system", "user"]
                        or any(set(message) != {"role", "content"} or not isinstance(message["content"], str)
                               for message in body["messages"])):
                    raise RuntimeError("production_request_shape_invalid")
                before = usage()
                # UTF-8 bytes upper-bound text tokens; extra framing/JSON bytes are conservative.
                reserve = ((Decimal(size + 256) * prompt_price
                            + Decimal(COMPLETION_CAP) * completion_price + request_price)
                           * Decimal("1.10"))
                conservative_usage = max(before, start + reported_total)
                if conservative_usage + reserve > CEILING:
                    raise RuntimeError("absolute_key_ceiling_reservation_refused")
                record = {
                    "sequence": len(accounting["calls"]) + 1,
                    "production_request_sha256": digest(original),
                    "system_prompt_sha256": digest(body["messages"][0]["content"].encode()),
                    "user_prompt_sha256": digest(body["messages"][1]["content"].encode()),
                    "key_usage_before_usd": float(before),
                    "reserved_usd": float(reserve),
                    "status": "submitted_unknown_liability",
                }
                accounting["calls"].append(record)
                # Persist the unknown reservation before any billable network operation.
                accounting["halted"] = True
                persist()
                body["max_completion_tokens"] = COMPLETION_CAP
                body["provider"] = accounting["provider_guards"]
                started = time.monotonic()
                response, raw = api("chat/completions", json.dumps(body, separators=(",", ":")).encode())
                record["latency_ms"] = round((time.monotonic() - started) * 1000, 3)
                with (args.out / f"raw-{record['sequence']:03d}.json").open("xb") as file:
                    file.write(raw)
                    file.flush()
                    os.fsync(file.fileno())
                charge = Decimal(str(response["usage"]["cost"]))
                if not charge.is_finite() or charge < 0 or charge > reserve:
                    raise RuntimeError("upstream_cost_missing_or_exceeds_reservation")
                after = usage()
                reported_total += charge
                upstream_id = response.get("id", "")
                record.update({
                    "status": "ok",
                    "provider_generation_id": upstream_id if re.fullmatch(r"[A-Za-z0-9_.:-]{1,160}", upstream_id) else None,
                    "reported_cost_usd": float(charge),
                    "input_tokens": response["usage"].get("prompt_tokens"),
                    "output_tokens": response["usage"].get("completion_tokens"),
                    "key_usage_after_usd": float(after),
                    "finish_reason": response["choices"][0].get("finish_reason"),
                })
                if (after < before or after > CEILING
                        or response["choices"][0].get("finish_reason") != "stop"
                        or not response["choices"][0]["message"].get("content")):
                    raise RuntimeError("counter_or_completion_guard_failed")
                accounting.update({"key_usage_end_usd": float(after),
                                   "key_usage_delta_usd": float(after - start),
                                   "reported_cost_usd": float(reported_total),
                                   "halted": False})
                persist()
                self.reply(200, response)
            except Exception as error:
                # Exception text and upstream bodies can contain sensitive request material.
                category = str(error) if isinstance(error, RuntimeError) else "broker_request_failed"
                if not re.fullmatch(r"[a-z_]+", category):
                    category = "broker_request_failed"
                accounting["halted"] = True
                if record is not None:
                    record["status"] = category
                accounting["halt_reason"] = category
                persist()
                self.reply(502, {"error": category})

    server = http.server.HTTPServer(("127.0.0.1", args.port), Handler)
    server.timeout = 120
    print("[eval-broker] ready loopback-only; credentials remain native", flush=True)
    try:
        server.serve_forever()
    finally:
        server.server_close()
        persist()


if __name__ == "__main__":
    try:
        main()
    except Exception:
        print("[eval-broker] startup_failed; no secret or upstream body logged", file=sys.stderr)
        sys.exit(1)
