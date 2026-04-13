#!/usr/bin/env python3
from __future__ import annotations

import argparse
import base64
import json
import os
import urllib.error
import urllib.parse
import urllib.request


API_BASE = "https://api.linode.com/v4"


def required_env(name: str) -> str:
    value = (os.environ.get(name) or "").strip()
    if not value:
        raise SystemExit(f"Missing required env var: {name}")
    return value


def write_outputs(values: dict[str, str], output_path: str | None) -> None:
    if not output_path:
        return

    with open(output_path, "a", encoding="utf-8") as handle:
        for key, value in values.items():
            handle.write(f"{key}={value}\n")


def linode_request(
    token: str,
    path: str,
    *,
    method: str = "GET",
    payload: dict | None = None,
) -> dict:
    request_data = None
    if payload is not None:
        request_data = json.dumps(payload).encode("utf-8")

    request = urllib.request.Request(f"{API_BASE}{path}", data=request_data, method=method)
    request.add_header("Authorization", f"Bearer {token}")
    if request_data is not None:
        request.add_header("Content-Type", "application/json")
        request.add_header("Content-Length", str(len(request_data)))

    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            body = response.read()
    except urllib.error.HTTPError as error:
        body = error.read().decode("utf-8", "replace")
        raise SystemExit(f"Linode API {error.code}: {body}") from error

    if not body:
        return {}
    return json.loads(body)


def list_paginated(token: str, path: str) -> list[dict]:
    query_separator = "&" if "?" in path else "?"
    page = 1
    items: list[dict] = []

    while True:
        payload = linode_request(
            token,
            f"{path}{query_separator}page={page}&page_size=100",
        )
        items.extend(payload.get("data", []))
        if page >= int(payload.get("pages", 1)):
            return items
        page += 1


def command_cluster_id(args: argparse.Namespace) -> int:
    token = required_env("LINODE_TOKEN")
    clusters = list_paginated(token, "/lke/clusters")
    for cluster in clusters:
        if cluster.get("label") != args.label:
            continue
        cluster_id = str(cluster["id"])
        print(cluster_id)
        write_outputs({"cluster_id": cluster_id}, args.github_output)
        return 0

    raise SystemExit(f"Could not find Linode LKE cluster with label: {args.label}")


def command_cluster_kubeconfig(args: argparse.Namespace) -> int:
    token = required_env("LINODE_TOKEN")
    response = linode_request(
        token,
        f"/lke/clusters/{urllib.parse.quote(args.cluster_id, safe='')}/kubeconfig",
    )
    encoded = (response.get("kubeconfig") or "").strip()
    if not encoded:
        raise SystemExit(f"Linode did not return kubeconfig for cluster {args.cluster_id}")

    kubeconfig = base64.b64decode(encoded)
    with open(args.output, "wb") as handle:
        handle.write(kubeconfig)

    print(args.output)
    write_outputs({"kubeconfig": args.output}, args.github_output)
    return 0


def resolve_domain_id(token: str, domain_name: str) -> int:
    domains = list_paginated(token, "/domains")
    for domain in domains:
        if domain.get("domain") == domain_name:
            return int(domain["id"])
    raise SystemExit(f"Could not find Linode domain: {domain_name}")


def command_upsert_domain_a_record(args: argparse.Namespace) -> int:
    token = required_env("LINODE_TOKEN")
    domain_id = resolve_domain_id(token, args.domain)
    records = list_paginated(token, f"/domains/{domain_id}/records")

    matching = [
        record
        for record in records
        if record.get("type") == "A" and record.get("name") == args.name
    ]

    payload = {
        "type": "A",
        "name": args.name,
        "target": args.target,
        "ttl_sec": args.ttl_sec,
    }

    if matching:
        first = matching[0]
        record_id = int(first["id"])
        linode_request(
            token,
            f"/domains/{domain_id}/records/{record_id}",
            method="PUT",
            payload=payload,
        )
        for duplicate in matching[1:]:
            linode_request(
                token,
                f"/domains/{domain_id}/records/{int(duplicate['id'])}",
                method="DELETE",
            )
        print(
            json.dumps(
                {
                    "domain_id": domain_id,
                    "record_id": record_id,
                    "domain": args.domain,
                    "name": args.name,
                    "target": args.target,
                    "ttl_sec": args.ttl_sec,
                    "action": "updated",
                }
            )
        )
        return 0

    response = linode_request(
        token,
        f"/domains/{domain_id}/records",
        method="POST",
        payload=payload,
    )
    print(
        json.dumps(
            {
                "domain_id": domain_id,
                "record_id": int(response["id"]),
                "domain": args.domain,
                "name": args.name,
                "target": args.target,
                "ttl_sec": args.ttl_sec,
                "action": "created",
            }
        )
    )
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    cluster_id = subparsers.add_parser("cluster-id")
    cluster_id.add_argument("--label", required=True)
    cluster_id.add_argument("--github-output")
    cluster_id.set_defaults(func=command_cluster_id)

    cluster_kubeconfig = subparsers.add_parser("cluster-kubeconfig")
    cluster_kubeconfig.add_argument("--cluster-id", required=True)
    cluster_kubeconfig.add_argument("--output", required=True)
    cluster_kubeconfig.add_argument("--github-output")
    cluster_kubeconfig.set_defaults(func=command_cluster_kubeconfig)

    upsert_domain = subparsers.add_parser("upsert-domain-a-record")
    upsert_domain.add_argument("--domain", required=True)
    upsert_domain.add_argument("--name", required=True)
    upsert_domain.add_argument("--target", required=True)
    upsert_domain.add_argument("--ttl-sec", type=int, default=30)
    upsert_domain.set_defaults(func=command_upsert_domain_a_record)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    return int(args.func(args))


if __name__ == "__main__":
    raise SystemExit(main())
