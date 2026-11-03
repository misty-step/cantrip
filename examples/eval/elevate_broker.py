#!/usr/bin/env python3
"""Native-only, loopback credential boundary for the production Elevate evaluator.

Run via pass-env with the dedicated SPEECH_EVAL entry, never on Boat. Reverse
forward this loopback port with `boat forward ID --reverse --local PORT --remote
PORT --bind 127.0.0.1`. Every upstream body originates in postproc::refine.
The optional comparison inserts only its precommitted fidelity addendum; all
requests receive identical max_completion_tokens and provider price guards.

--inspect reads the live counter/catalog without inference. --out must be fresh,
private scratch outside the repo. Only accounting.json is publishable; raw-*.json
contain model outputs and must remain private. No headers, keys or transcript
text are printed. A refused request, unknown cost, interrupted request or missing
post-call counter halts the broker; a new invocation is not a paid retry permit.
--resume accepts only fully charged, nonempty, stop-completed checkpoints. Their
byte-identical production request hashes replay retained responses locally; they
are never submitted upstream again. Backwards counter reads retain the observed
high-water mark and all known charges rather than manufacturing refunded room.
--comparison-plan requires one opaque per-slot endpoint for all 246 precommitted
samples. Slot identity, arm, repeat and exact production hash distinguish fresh
repeats from replay; identical bodies never merge independently scheduled draws.
"""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import sys
import shutil
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
    parser.add_argument("--resume", type=Path)
    parser.add_argument("--comparison-plan", type=Path)
    parser.add_argument("--operator-prompt", type=Path)
    parser.add_argument("--candidate-addendum", type=Path)
    parser.add_argument("--base-system-sha256")
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
                                          "completion": float(completion_price * 1_000_000),
                                          "request": float(request_price)}},
        "counter_policy": "Read before and after every serial call; budget against max(counter high-water, initial usage plus all known provider charges). Reserve byte-bounded input plus 256 framing tokens and 1024 completion tokens at capped prices with a 10% margin. A backwards read never refunds room. Unknown liability halts; no paid retries.",
        "credential_placement": "Native pass-env process only; reverse-forwarded loopback HTTP carries no authorization header from the worker.",
        "calls": [],
        "halted": start >= CEILING,
    }
    plan = None
    plan_calls = []
    operator = None
    addendum = None
    if args.comparison_plan is not None:
        if args.operator_prompt is None or args.candidate_addendum is None or args.base_system_sha256 is None:
            raise RuntimeError("comparison_prompt_assets_required")
        plan_bytes = args.comparison_plan.read_bytes()
        plan = json.loads(plan_bytes)
        operator_bytes = args.operator_prompt.read_bytes()
        addendum_bytes = args.candidate_addendum.read_bytes()
        if (plan["schema"] != "cantrip.eval.elevate-comparison-plan.v1"
                or len(plan["calls"]) != 246
                or len(addendum_bytes) != 263 or len(addendum_bytes.splitlines()) != 3
                or digest(operator_bytes) != plan["operator_sha256"]
                or digest(addendum_bytes) != plan["addendum_sha256"]):
            raise RuntimeError("comparison_precommit_mismatch")
        plan_calls = plan["calls"]
        slots = set()
        coverage = {}
        for slot in plan_calls:
            if (not re.fullmatch(r"[0-9a-f]{32}", slot["id"]) or slot["id"] in slots
                    or slot["arm"] not in ("operator", "candidate") or slot["iteration"] not in (1, 2, 3)):
                raise RuntimeError("comparison_schedule_invalid")
            slots.add(slot["id"])
            key_tuple = (slot["case"], slot["iteration"], slot["arm"])
            coverage[key_tuple] = coverage.get(key_tuple, 0) + 1
        if len({slot["case"] for slot in plan_calls}) != 41 or any(count != 1 for count in coverage.values()):
            raise RuntimeError("comparison_schedule_coverage_invalid")
        operator = operator_bytes.decode("utf-8")
        addendum = addendum_bytes.decode("utf-8")
        accounting["comparison"] = {
            "plan_sha256": digest(plan_bytes),
            "worker_schedule_sha256": plan["worker_schedule_sha256"],
            "operator_sha256": digest(operator_bytes),
            "addendum_sha256": digest(addendum_bytes),
            "addendum_bytes": len(addendum_bytes),
            "base_system_sha256": args.base_system_sha256,
            "planned_calls": 246,
            "seed": plan["seed"],
        }
    reported_total = Decimal(0)
    high_water = start
    replays = []
    replay_index = 0
    if args.resume is not None:
        previous = json.loads((args.resume / "accounting.json").read_bytes())
        if (previous["model"] != MODEL or previous["reasoning_effort"] != "low"
                or previous["provider_guards"] != accounting["provider_guards"]
                or previous["test_only_max_completion_tokens"] != COMPLETION_CAP
                or previous.get("comparison") != accounting.get("comparison")):
            raise RuntimeError("checkpoint_protocol_mismatch")
        fresh_counter = start
        start = Decimal(str(previous["key_usage_start_usd"]))
        for record in previous["calls"]:
            raw = (args.resume / f"raw-{record['sequence']:03d}.json").read_bytes()
            response = json.loads(raw)
            charge = Decimal(str(response["usage"]["cost"]))
            if (not charge.is_finite() or charge < 0
                    or charge != Decimal(str(record["reported_cost_usd"]))
                    or charge > Decimal(str(record["reserved_usd"]))
                    or response["choices"][0].get("finish_reason") != "stop"
                    or not response["choices"][0]["message"].get("content")
                    or record["status"] not in ("ok", "counter_or_completion_guard_failed", "reconciled_complete")):
                raise RuntimeError("checkpoint_has_unknown_or_incomplete_liability")
            reported_total += charge
            high_water = max(high_water, Decimal(str(record["key_usage_before_usd"])),
                             Decimal(str(record["key_usage_after_usd"])))
            replays.append((record.get("schedule_slot"), record["production_request_sha256"], response))
            shutil.copyfile(args.resume / f"raw-{record['sequence']:03d}.json",
                            args.out / f"raw-{record['sequence']:03d}.json")
        if fresh_counter < start + reported_total:
            raise RuntimeError("checkpoint_counter_not_reconciled")
        policy = accounting["counter_policy"]
        accounting = previous
        accounting.update({
            "counter_policy": policy,
            "interrupted_attempt": {
                "halt_reason": previous.get("halt_reason"),
                "cloud_submissions": len(replays),
                "reconciled_key_usage_usd": float(fresh_counter),
                "reported_cost_usd": float(reported_total),
                "recovery": "Byte-identical production request hashes replay retained completed responses locally; no duplicate upstream submission.",
            },
            "halted": False,
            "reported_cost_usd": float(reported_total),
            "local_replays": 0,
        })
        accounting.pop("halt_reason", None)
        for record in accounting["calls"]:
            if record["status"] != "ok":
                record["prior_status"] = record["status"]
                record["status"] = "reconciled_complete"
    accounting["key_usage_high_water_usd"] = float(high_water)

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
            nonlocal reported_total, high_water, replay_index
            route = re.fullmatch(r"/api/v1/eval/([0-9a-f]{32})/chat/completions", self.path)
            route_allowed = route is not None if plan is not None else self.path == "/api/v1/chat/completions"
            if not route_allowed or accounting["halted"]:
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
                request_hash = digest(original)
                slot = None
                if plan is not None:
                    next_index = replay_index if replay_index < len(replays) else len(accounting["calls"])
                    if next_index >= len(plan_calls):
                        raise RuntimeError("comparison_complete_no_more_submissions")
                    slot = plan_calls[next_index]
                    if route.group(1) != slot["id"]:
                        raise RuntimeError("comparison_slot_order_mismatch")
                    system = body["messages"][0]["content"]
                    if digest(system.encode()) != args.base_system_sha256 or not system.startswith(operator):
                        raise RuntimeError("comparison_production_system_mismatch")
                    if slot["arm"] == "candidate":
                        body["messages"][0]["content"] = operator + "\n" + addendum + system[len(operator):]
                if replay_index < len(replays):
                    expected_slot, expected_hash, retained = replays[replay_index]
                    if request_hash != expected_hash or slot != expected_slot:
                        raise RuntimeError("checkpoint_production_request_hash_mismatch")
                    replay_index += 1
                    accounting["local_replays"] = replay_index
                    persist()
                    self.reply(200, retained)
                    return
                if any((prior.get("schedule_slot") == slot if plan is not None
                        else prior["production_request_sha256"] == request_hash) for prior in accounting["calls"]):
                    raise RuntimeError("duplicate_upstream_submission_refused")
                before = usage()
                evaluated = json.dumps(body, separators=(",", ":")).encode()
                # UTF-8 bytes upper-bound text tokens; extra framing/JSON bytes are conservative.
                reserve = ((Decimal(max(size, len(evaluated)) + 256) * prompt_price
                            + Decimal(COMPLETION_CAP) * completion_price + request_price)
                           * Decimal("1.10"))
                high_water = max(high_water, before)
                conservative_usage = max(high_water, start + reported_total)
                if conservative_usage + reserve > CEILING:
                    raise RuntimeError("absolute_key_ceiling_reservation_refused")
                record = {
                    "sequence": len(accounting["calls"]) + 1,
                    "schedule_slot": slot,
                    "production_request_sha256": request_hash,
                    "evaluated_request_sha256": digest(evaluated),
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
                high_water = max(high_water, after)
                upstream_id = response.get("id", "")
                record.update({
                    "status": "ok",
                    "provider_generation_id": upstream_id if re.fullmatch(r"[A-Za-z0-9_.:-]{1,160}", upstream_id) else None,
                    "reported_cost_usd": float(charge),
                    "input_tokens": response["usage"].get("prompt_tokens"),
                    "output_tokens": response["usage"].get("completion_tokens"),
                    "key_usage_after_usd": float(after),
                    "counter_read_regressed": after < before,
                    "finish_reason": response["choices"][0].get("finish_reason"),
                })
                if (max(high_water, start + reported_total) > CEILING
                        or response["choices"][0].get("finish_reason") != "stop"
                        or not response["choices"][0]["message"].get("content")):
                    raise RuntimeError("counter_or_completion_guard_failed")
                accounting.update({"key_usage_end_usd": float(after),
                                   "key_usage_delta_usd": float(after - start),
                                   "key_usage_high_water_usd": float(high_water),
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
