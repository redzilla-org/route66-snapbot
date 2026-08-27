#!/usr/bin/env python3
"""Deploy DEV-account read roles used by the command-center evidence attestor.

WHY THIS IS SEPARATE FROM deploy.py: the attestor stack lives in command-center,
but the CI source of truth lives in each DEV account's Step Functions execution
history. This script installs the narrow cross-account role in california-dev and
chicago-dev so the command-center Lambda can verify target commits itself.
"""
import sys
import time
from pathlib import Path

import boto3
import botocore

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from scripts.lib.r66 import account_id, all_env_names, aws_profile, is_prod, region  # noqa: E402

HERE = Path(__file__).resolve().parent
TEMPLATE = HERE / "dev-ci-read-role-template.yaml"

# The dev-env table comes from route66's ONE env table (scripts/lib/r66), the
# same source deploy.py feeds the attestor's DevCiTargetsJson parameter from
# (owner 2026-08-26: no hand-copied account tables).
ENVS = {
    env: {"profile": aws_profile(env), "region": region(env), "account": account_id(env)}
    for env in all_env_names() if env.endswith("-dev") and not is_prod(env)
}

STACK_NAME = "route66-evidence-attestor-ci-read"


def template_body() -> str:
    """Read the raw CloudFormation role template."""
    return TEMPLATE.read_text(encoding="utf-8")


def assert_account(sess, env: str, want: str) -> None:
    """Fail closed if a profile points at the wrong AWS account."""
    got = sess.client("sts").get_caller_identity()
    if got["Account"] != want:
        sys.exit(f"REFUSING {env}: profile resolved account {got['Account']}, want {want}")
    print(f"{env}: account OK {got['Account']} ({got['Arn']})")


def stack_exists(cfn) -> bool:
    """Return whether the role stack already exists."""
    try:
        cfn.describe_stacks(StackName=STACK_NAME)
        return True
    except botocore.exceptions.ClientError as exc:
        if "does not exist" in str(exc):
            return False
        raise


def wait(cfn, waiter: str) -> None:
    """Wait for CloudFormation to finish the create/update."""
    cfn.get_waiter(waiter).wait(StackName=STACK_NAME, WaiterConfig={"Delay": 8, "MaxAttempts": 60})


def deploy_env(env: str, cfg: dict) -> None:
    """Create or update the one narrow role stack in a DEV account."""
    sess = boto3.Session(profile_name=cfg["profile"], region_name=cfg["region"])
    assert_account(sess, env, cfg["account"])
    cfn = sess.client("cloudformation")
    params = [{"ParameterKey": "EnvironmentName", "ParameterValue": env}]
    kwargs = {
        "StackName": STACK_NAME,
        "TemplateBody": template_body(),
        "Parameters": params,
        "Capabilities": ["CAPABILITY_NAMED_IAM"],
    }
    if stack_exists(cfn):
        print(f"{env}: updating {STACK_NAME}")
        try:
            cfn.update_stack(**kwargs)
        except botocore.exceptions.ClientError as exc:
            if "No updates are to be performed" in str(exc):
                print(f"{env}: no changes")
                return
            raise
        wait(cfn, "stack_update_complete")
    else:
        print(f"{env}: creating {STACK_NAME}")
        cfn.create_stack(OnFailure="DELETE", **kwargs)
        wait(cfn, "stack_create_complete")
    print(f"{env}: {STACK_NAME} deployed")


def main() -> int:
    """Deploy both DEV roles unless specific env names are supplied."""
    targets = sys.argv[1:] or list(ENVS)
    for env in targets:
        if env not in ENVS:
            sys.exit(f"unknown env {env}; expected one of {', '.join(ENVS)}")
        deploy_env(env, ENVS[env])
        time.sleep(1)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
