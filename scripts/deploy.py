"""Apply runtime secrets and an immutable image without writing secrets to disk."""

import base64
import hashlib
import json
import os
import re
import subprocess
import sys
from urllib.parse import urlparse


def kubectl(*args, document=None):
    result = subprocess.run(
        ["kubectl", "--namespace=jeeves", *args],
        input=json.dumps(document) if document is not None else None,
        text=True,
        capture_output=True,
    )
    if result.returncode:
        # Kubernetes validation errors can echo submitted secret data.
        sys.exit("Kubernetes operation failed; inspect deployment status without dumping secrets.")
    return result.stdout


def main():
    image = os.environ["JEEVES_IMAGE"]
    if not re.fullmatch(r"[a-z0-9./_-]+@sha256:[a-f0-9]{64}", image):
        sys.exit("JEEVES_IMAGE must be a registry image pinned by SHA-256 digest.")

    names = (
        "DISCORD_TOKEN", "DATABASE_URL", "DATABASE_MAX_CONNECTIONS", "TYPESAFE_API_KEY",
        "TYPESAFE_MODEL", "GEMINI_BACKEND", "GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION",
        "GEMINI_MODEL", "GEMINI_API_KEY", "RUST_LOG",
        "TWITCH_CLIENT_ID", "TWITCH_ACCESS_TOKEN", "TWITCH_CLIENT_SECRET",
        "TWITCH_BOT_LOGIN", "PUBLIC_URL",
    )
    values = {name: os.environ[name] for name in names if os.environ.get(name)}
    required = {"DATABASE_URL", "TYPESAFE_API_KEY"}
    hosted = bool(values.get("TWITCH_CLIENT_SECRET") or values.get("TWITCH_BOT_LOGIN"))
    twitch_names = {"TWITCH_CLIENT_ID", "TWITCH_ACCESS_TOKEN", "TWITCH_CLIENT_SECRET", "TWITCH_BOT_LOGIN"}
    if twitch_names & values.keys():
        required.add("TWITCH_CLIENT_ID")
        required.update({"TWITCH_CLIENT_SECRET", "TWITCH_BOT_LOGIN", "PUBLIC_URL"} if hosted else {"TWITCH_ACCESS_TOKEN"})
    elif not values.get("DISCORD_TOKEN"):
        sys.exit("Configure DISCORD_TOKEN or Twitch runtime secrets.")
    if values.get("GEMINI_BACKEND", "developer") == "vertex":
        required.add("GOOGLE_CLOUD_PROJECT")
    elif values.get("GEMINI_BACKEND", "developer") == "developer":
        required.add("GEMINI_API_KEY")
    else:
        sys.exit("GEMINI_BACKEND must be vertex or developer.")
    missing = sorted(required - values.keys())
    if missing:
        sys.exit("Missing runtime secrets: " + ", ".join(missing))

    web = None
    if hosted:
        origin = urlparse(values["PUBLIC_URL"])
        if (origin.scheme != "https" or not origin.hostname or origin.username or origin.password
                or origin.port or origin.path not in ("", "/") or origin.query or origin.fragment):
            sys.exit("PUBLIC_URL must be an HTTPS origin without a port or path.")
        if not re.fullmatch(r"[a-z0-9_]{1,25}", values["TWITCH_BOT_LOGIN"]):
            sys.exit("TWITCH_BOT_LOGIN must be a lowercase Twitch login.")
        web = json.loads(kubectl(
            "create", "--dry-run=client", "--validate=false", "-f", "deploy/web.yaml", "-o", "json"
        ))
        ingress = next(item for item in web["items"] if item["kind"] == "Ingress")
        ingress["spec"]["rules"][0]["host"] = origin.hostname
        ingress["spec"]["tls"][0]["hosts"] = [origin.hostname]

    # Parse the checked-in manifest before changing any runtime configuration.
    deployment = json.loads(kubectl(
        "create", "--dry-run=client", "--validate=false", "-f", "deploy/deployment.yaml", "-o", "json"
    ))
    template = deployment["spec"]["template"]
    template["spec"]["containers"][0]["image"] = image
    if not hosted:
        template["spec"]["containers"][0].pop("ports", None)
        template["spec"]["containers"][0].pop("readinessProbe", None)
    template["metadata"]["annotations"] = {
        "jeeves.infrawrench.com/config-sha256": hashlib.sha256(
            json.dumps(values, sort_keys=True).encode()
        ).hexdigest()
    }
    secret = {
        "apiVersion": "v1", "kind": "Secret", "type": "Opaque",
        "metadata": {"name": "jeeves-env", "namespace": "jeeves"},
        "data": {name: base64.b64encode(value.encode()).decode() for name, value in values.items()},
    }
    kubectl("apply", "--server-side", "--field-manager=jeeves-ci", "-f", "-", document=secret)
    kubectl("apply", "--server-side", "--field-manager=jeeves-ci", "-f", "-", document=deployment)
    if web:
        kubectl("apply", "--server-side", "--field-manager=jeeves-ci", "-f", "-", document=web)
    print("Applied runtime configuration and deployment.", flush=True)
    subprocess.run(
        ["kubectl", "--namespace=jeeves", "rollout", "status", "deployment/jeeves", "--timeout=600s"],
        check=True,
    )


if __name__ == "__main__":
    main()
