"""Apply runtime secrets and an immutable image without writing secrets to disk."""

import base64
import hashlib
import json
import os
import re
import subprocess
import sys


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
    )
    values = {name: os.environ[name] for name in names if os.environ.get(name)}
    required = {"DISCORD_TOKEN", "DATABASE_URL", "TYPESAFE_API_KEY", "GEMINI_BACKEND"}
    if values.get("GEMINI_BACKEND") == "vertex":
        required.add("GOOGLE_CLOUD_PROJECT")
    elif values.get("GEMINI_BACKEND") == "developer":
        required.add("GEMINI_API_KEY")
    else:
        sys.exit("GEMINI_BACKEND must be vertex or developer.")
    missing = sorted(required - values.keys())
    if missing:
        sys.exit("Missing runtime secrets: " + ", ".join(missing))

    # Parse the checked-in manifest before changing any runtime configuration.
    deployment = json.loads(kubectl(
        "create", "--dry-run=client", "--validate=false", "-f", "deploy/deployment.yaml", "-o", "json"
    ))
    template = deployment["spec"]["template"]
    template["spec"]["containers"][0]["image"] = image
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
    print("Applied runtime configuration and deployment.", flush=True)
    subprocess.run(
        ["kubectl", "--namespace=jeeves", "rollout", "status", "deployment/jeeves", "--timeout=600s"],
        check=True,
    )


if __name__ == "__main__":
    main()
