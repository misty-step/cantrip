#!/usr/bin/env python3
"""Prepare one paired Elevate trial, blind completed outputs, then decode fixed grades.

No model requests or credentials are handled here. postproc::refine and the native
broker own inference. Keep every intermediate under private scratch outside the
repo. Give the independent grader only blind/items.json, never native-plan.json
or the decode file. After fixed hash-bound grades, aggregate emits a compact
comparison.json and a private numeric per-call receipt; history stays unchanged.
"""

import argparse
from collections import Counter, defaultdict
from decimal import Decimal
import hashlib
import json
import math
import os
from pathlib import Path
import random

CANDIDATE_SHA256 = "05f0cc50c043c09663246229879f2edacadf2ebf4dc0dfbc25a5deb6d233b742"
FIDELITY = {"speech_act", "fact_added", "fact_omitted", "number", "negation", "modality", "literal"}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def load(path):
    return json.loads(path.read_bytes())


def save(path, data):
    with path.open("x", encoding="utf-8") as file:
        json.dump(data, file, indent=2)
        file.write("\n")
        file.flush()
        os.fsync(file.fileno())


def fresh(path):
    if not path.is_absolute() or path.resolve().is_relative_to(Path.cwd().resolve()):
        raise ValueError("private_scratch_outside_repository_required")
    os.umask(0o077)
    path.mkdir(mode=0o700)


def corpus(path):
    manifest = load(path)
    inherited = load(path.parent / manifest["include"])
    cases = inherited["cases"] + manifest["cases"]
    if len(inherited["cases"]) != 33 or len(manifest["cases"]) != 8 or len({case["id"] for case in cases}) != 41:
        raise ValueError("fixed_41_case_corpus_required")
    return manifest, {case["id"]: case for case in cases}


def prepare(args):
    fresh(args.out)
    _, cases = corpus(args.manifest)
    operator = args.operator_prompt.read_bytes()
    addendum = args.candidate_addendum.read_bytes()
    if len(addendum) != 263 or len(addendum.splitlines()) != 3 or digest(addendum) != CANDIDATE_SHA256:
        raise ValueError("candidate_precommit_mismatch")
    rng = random.Random(args.seed)
    blocks = [(case, repeat) for case in cases for repeat in range(1, 4)]
    rng.shuffle(blocks)
    slots = []
    for case, repeat in blocks:
        arms = ["operator", "candidate"]
        rng.shuffle(arms)
        for arm in arms:
            slots.append({"id": f"{rng.getrandbits(128):032x}", "case": case, "iteration": repeat, "arm": arm})
    worker = {"schema": "cantrip.eval.elevate-schedule.v1", "calls": [{key: slot[key] for key in ("id", "case", "iteration")} for slot in slots]}
    save(args.out / "worker-schedule.json", worker)
    plan = {
        "schema": "cantrip.eval.elevate-comparison-plan.v1",
        "seed": args.seed,
        "ordering": "Shuffle case/repeat pairs, then randomize the two arm positions within every pair.",
        "worker_schedule_sha256": digest((args.out / "worker-schedule.json").read_bytes()),
        "operator_sha256": digest(operator),
        "addendum_sha256": digest(addendum),
        "manifest_sha256": digest(args.manifest.read_bytes()),
        "inherited_manifest_sha256": digest((args.manifest.parent / load(args.manifest)["include"]).read_bytes()),
        "precommit": {
            "candidate_revision": "61dfe40", "comparison_rule_revision": "f53020d",
            "planned_upstream_submissions": 246, "cases": 41, "repeats": 3,
            "model": "openai/gpt-6.1-sol", "reasoning_effort": "low", "absolute_key_ceiling_usd": 4.5,
            "primary": "One binary fidelity-failed outcome per output if any blind semantic fidelity category fails. No Clean wording oracle; numeric values, meaningful modality/conditions and literal content are protected, not unquoted number spelling, quotation wrappers or verbal filler.",
            "fidelity_categories": sorted(FIDELITY),
            "lexical_floor_per_arm": "21/24 across the eight added cases and three repeats",
            "selection": "Among complete arms meeting the lexical floor, choose fewer fidelity-failed outputs. Ties choose operator. Protocol failures never become passes.",
            "historical_outputs_reused": 0, "paid_retries_authorized": False,
        },
        "calls": slots,
    }
    save(args.out / "native-plan.json", plan)
    print("[eval] paired comparison prepared planned=246; decode metadata remains private")


def completed(args):
    plan = load(args.plan)
    run = load(args.run / "receipt.json")
    outputs = load(args.run / "private.json")
    broker = load(args.broker / "accounting.json")
    if len(plan["calls"]) != 246 or len(outputs) != 246 or len(broker["calls"]) != 246 or len(run["calls"]) != 246:
        raise ValueError("comparison_incomplete")
    if len({call["schedule_slot"]["id"] for call in broker["calls"]}) != 246:
        raise ValueError("duplicate_or_missing_scheduled_upstream_submission")
    for slot, output, receipt, upstream in zip(plan["calls"], outputs, run["calls"], broker["calls"]):
        if (slot != upstream["schedule_slot"] or slot["id"] != output["schedule_id"]
                or slot["id"] != receipt["schedule_id"] or slot["case"] != output["case"]
                or slot["iteration"] != output["iteration"] or "request" in receipt["failures"]
                or output["output_sha256"] != digest(output["text"].encode())
                or receipt["output_sha256"] != output["output_sha256"]
                or upstream["finish_reason"] != "stop" or upstream["status"] not in ("ok", "reconciled_complete")):
            raise ValueError("comparison_output_or_protocol_mismatch")
    return plan, run, outputs, broker


def blind(args):
    plan, _, outputs, _ = completed(args)
    _, cases = corpus(args.manifest)
    fresh(args.out)
    rng = random.Random(plan["seed"] ^ 0xA17E6B11)
    unique = {}
    decode = {}
    for slot, output in zip(plan["calls"], outputs):
        targets = cases[slot["case"]].get("checks", {}).get("lexical_any", [])
        key = (output["alias_corrected_source"], output["text"], tuple(targets))
        if key not in unique:
            item_id = f"{rng.getrandbits(128):032x}"
            unique[key] = {"id": item_id, "source": key[0], "candidate": key[1],
                           "output_sha256": output["output_sha256"], "lexical_targets": targets}
            decode[item_id] = {"output_sha256": output["output_sha256"], "lexical_targets": targets, "slots": []}
        decode[unique[key]["id"]]["slots"].append(slot["id"])
    items = list(unique.values())
    rng.shuffle(items)
    artifact = {"schema": "cantrip.eval.blinded-candidates.v1", "items": items}
    save(args.out / "items.json", artifact)
    save(args.decode_out, {"schema": "cantrip.eval.blind-decode.v1",
                           "blinded_artifact_sha256": digest((args.out / "items.json").read_bytes()),
                           "items": decode})
    print(f"[eval] blinded artifact ready items={len(items)} weighted_outputs=246")


def percentiles(values):
    values = sorted(values)
    return {"n": len(values), "mean": sum(values) / len(values), "p50": values[math.ceil(len(values)*.50)-1],
            "p95": values[math.ceil(len(values)*.95)-1], "min": values[0], "max": values[-1]}


def aggregate(args):
    plan, run, outputs, broker = completed(args)
    decode = load(args.decode)
    grades = load(args.judgments)
    if grades["schema"] != "cantrip.eval.blind-judgments.v1" or not grades["reviewer"].strip():
        raise ValueError("independent_blind_judgments_required")
    judgments = {}
    slot_grades = {}
    for judgment in grades["items"]:
        item_id = judgment["id"]
        if item_id in judgments or item_id not in decode["items"]:
            raise ValueError("duplicate_or_unknown_blind_item")
        item = decode["items"][item_id]
        failures = judgment["fidelity_failures"]
        lexical = judgment["lexical_upgrade"]
        if (judgment["output_sha256"] != item["output_sha256"] or set(failures) - FIDELITY
                or len(failures) != len(set(failures))
                or (item["lexical_targets"] and type(lexical) is not bool)
                or (not item["lexical_targets"] and lexical is not None)):
            raise ValueError("blind_judgment_hash_or_schema_mismatch")
        judgments[item_id] = judgment
        for slot_id in item["slots"]:
            if slot_id in slot_grades:
                raise ValueError("slot_mapped_twice")
            slot_grades[slot_id] = (item_id, judgment)
    if set(judgments) != set(decode["items"]) or len(slot_grades) != 246:
        raise ValueError("every_blinded_output_requires_a_fixed_judgment")
    fresh(args.out)
    per_arm = defaultdict(list)
    public_calls = []
    for slot, output, receipt, upstream in zip(plan["calls"], outputs, run["calls"], broker["calls"]):
        item_id, judgment = slot_grades[slot["id"]]
        charge = Decimal(str(upstream["reported_cost_usd"]))
        if charge != Decimal(str(receipt["cost_usd"])):
            raise ValueError("production_usage_broker_cost_mismatch")
        call = {"case": slot["case"], "repeat": slot["iteration"], "arm": slot["arm"],
                "blind_item_id": item_id, "output_sha256": output["output_sha256"],
                "fidelity_failures": judgment["fidelity_failures"],
                "fidelity_failed": bool(judgment["fidelity_failures"]),
                "lexical_upgrade": judgment["lexical_upgrade"],
                "deterministic_diagnostics_only": receipt["failures"],
                "upstream_latency_ms": upstream["latency_ms"], "cost_usd": float(charge),
                "input_tokens": upstream["input_tokens"], "output_tokens": upstream["output_tokens"],
                "provider_generation_id": upstream["provider_generation_id"],
                "production_request_sha256": upstream["production_request_sha256"],
                "evaluated_request_sha256": upstream["evaluated_request_sha256"],
                "system_prompt_sha256": upstream["system_prompt_sha256"]}
        public_calls.append(call)
        per_arm[slot["arm"]].append(call)
    arms = {}
    for arm, calls in per_arm.items():
        lexical = [call for call in calls if call["lexical_upgrade"] is not None]
        lex_success = sum(call["lexical_upgrade"] for call in lexical)
        arms[arm] = {
            "outputs": len(calls), "protocol_failures": 0,
            "fidelity_failed_outputs": sum(call["fidelity_failed"] for call in calls),
            "fidelity_passed_outputs": sum(not call["fidelity_failed"] for call in calls),
            "failure_categories": dict(sorted(Counter(category for call in calls for category in call["fidelity_failures"]).items())),
            "lexical_successes": lex_success, "lexical_possible": 24,
            "lexical_by_repeat": {str(repeat):sum(call["lexical_upgrade"] for call in lexical if call["repeat"] == repeat) for repeat in range(1,4)},
            "lexical_by_case": {case:sum(call["lexical_upgrade"] for call in lexical if call["case"] == case) for case in sorted({call["case"] for call in lexical})},
            "lexical_distinct_case_coverage": len({call["case"] for call in lexical if call["lexical_upgrade"]}),
            "qualifies": len(calls) == 123 and lex_success >= 21,
            "upstream_latency_ms": percentiles([call["upstream_latency_ms"] for call in calls]),
            "reported_cost_usd": float(sum((Decimal(str(call["cost_usd"])) for call in calls), Decimal(0))),
            "input_tokens": sum(call["input_tokens"] for call in calls),
            "output_tokens": sum(call["output_tokens"] for call in calls),
        }
    eligible = [arm for arm in ("operator", "candidate") if arms[arm]["qualifies"]]
    selected = min(eligible, key=lambda arm:(arms[arm]["fidelity_failed_outputs"], arm != "operator")) if eligible else None
    total = sum((Decimal(str(call["cost_usd"])) for call in public_calls), Decimal(0))
    final = load(args.final_counter)
    start = Decimal(str(broker["key_usage_start_usd"]))
    end = Decimal(str(final["key_usage_start_usd"]))
    if end - start != total or end > Decimal("4.50"):
        raise ValueError("final_key_counter_not_reconciled")
    generation_ids = {call["provider_generation_id"] for call in public_calls}
    if None in generation_ids or len(generation_ids) != 246:
        raise ValueError("every_scheduled_draw_requires_a_unique_provider_generation")
    save(args.out / "private-calls.json", public_calls)
    comparison = {
        "schema": "cantrip.eval.elevate-comparison.v1", "run_id": "2026-11-03-elevate-ab",
        "claim_type": "Measured execution/accounting; quality counts are independent blinded reviewer judgments, not builder labels.",
        "source_revision": run["source_revision"], "precommit": plan["precommit"],
        "aggregation_script_sha256": digest(Path(__file__).read_bytes()),
        "ordering": plan["ordering"], "seed": plan["seed"],
        "corpus_sha256": plan["manifest_sha256"], "inherited_corpus_sha256": plan["inherited_manifest_sha256"],
        "operator_sha256": plan["operator_sha256"], "addendum_sha256": plan["addendum_sha256"],
        "system_prompt_sha256": {arm:sorted({call["system_prompt_sha256"] for call in calls}) for arm,calls in per_arm.items()},
        "blind_review": {"reviewer": grades["reviewer"], "artifact_sha256": decode["blinded_artifact_sha256"],
                         "judgments_sha256": digest(args.judgments.read_bytes()), "unique_items": len(judgments),
                         "weighted_outputs":246, "decode_revealed_only_after_fixed_grades":True},
        "arms": arms, "selected_arm": selected,
        "accounting": {"unique_scheduled_upstream_submissions":246, "historical_outputs_reused":0,
                       "paid_retries":0, "reported_cost_usd":float(total),
                       "canonical_model":broker["canonical_model"],
                       "key_usage_start_usd":float(start), "key_usage_end_usd":float(end),
                       "absolute_key_ceiling_usd":4.5, "test_only_max_completion_tokens":1024,
                       "provider_guards":broker["provider_guards"], "counter_policy":broker["counter_policy"],
                       "local_completed_response_replays":broker.get("local_replays",0),
                       "interruption":broker.get("interrupted_attempt")},
        "failures": [{key:call[key] for key in ("case", "repeat", "arm", "blind_item_id", "output_sha256", "fidelity_failures")}
                     for call in public_calls if call["fidelity_failed"]],
        "sampling_provenance": {
            "unique_provider_generations":len(generation_ids),
            "unique_original_production_bodies":len({call["production_request_sha256"] for call in public_calls}),
            "unique_effective_bodies":len({call["evaluated_request_sha256"] for call in public_calls}),
            "private_numeric_calls_sha256":digest((args.out / "private-calls.json").read_bytes()),
            "private_production_receipt_sha256":digest((args.run / "receipt.json").read_bytes()),
            "private_provider_receipt_sha256":digest((args.broker / "accounting.json").read_bytes()),
        },
        "limitations": ["One precommitted 41-case x3-repeat x2-arm synthetic trial; no private dictation, native UI or reliability claim.",
                        "The same decision corpus informed the initial study and is not a held-out or independent dictation corpus.",
                        "Identical source/output pairs were deduplicated for blind judgment and then weighted back to every scheduled output.",
                        "Primary fidelity and lexical scores are the independent blind reviewer's judgments, not Clean wording or builder exact-number diagnostics.",
                        "The candidate system insertion and identical generation/price caps were broker-only transport changes; production postproc::refine generated every original request."]
    }
    if args.execution is not None:
        comparison["execution"] = load(args.execution)
    save(args.out / "comparison.json", comparison)
    print("[eval] comparison decoded after fixed blind judgments; text-free receipt ready")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prep = commands.add_parser("prepare")
    prep.add_argument("--out", type=Path, required=True)
    prep.add_argument("--manifest", type=Path, default=Path("samples/eval/elevate-behavior.json"))
    prep.add_argument("--operator-prompt", type=Path, required=True)
    prep.add_argument("--candidate-addendum", type=Path, required=True)
    prep.add_argument("--seed", type=int, required=True)
    prep.set_defaults(action=prepare)
    for command, function in (("blind", blind), ("aggregate", aggregate)):
        sub = commands.add_parser(command)
        sub.add_argument("--plan", type=Path, required=True)
        sub.add_argument("--run", type=Path, required=True)
        sub.add_argument("--broker", type=Path, required=True)
        sub.add_argument("--out", type=Path, required=True)
        sub.set_defaults(action=function)
        if command == "blind":
            sub.add_argument("--manifest", type=Path, default=Path("samples/eval/elevate-behavior.json"))
            sub.add_argument("--decode-out", type=Path, required=True)
        else:
            sub.add_argument("--decode", type=Path, required=True)
            sub.add_argument("--judgments", type=Path, required=True)
            sub.add_argument("--final-counter", type=Path, required=True)
            sub.add_argument("--execution", type=Path)
    args = parser.parse_args()
    args.action(args)


if __name__ == "__main__":
    main()
