#!/usr/bin/env python3
"""Build and deploy the command-center route66-snapbot image.

boto3 plus Docker, fixed command-center account/region, immutable SHA image tag,
and live preflight checks before any stack mutation.

KEY MATERIAL (owner 2026-08-26, "the AWS key should be hard-coded (not Lambda
env)"): the AES unwrap key is the UNWRAP_KEY_B64 constant in attestor/index.js;
the wrapped Ed25519 seed is the plain SSM String /r66/evidence-attestor/
private-key.v1. This driver reads the constant out of index.js, unwraps the
SSM value with it, and refuses to deploy unless the seed derives to the
committed public-key.json -- the guard that keeps a silent key mismatch from
producing signatures no verifier accepts. On a first deploy the SSM value comes
from tmp/command-center-evidence-attestor-bootstrap.json ("ssm_string_value").

ARTIFACT BUCKET (owner 2026-08-26): the Lambda zip is staged in the private
command-center DevOps bucket created by devops-bucket-template.yaml
(core-devops-bucket-command-center-<acct>-<region>), never in the public
evidence bucket.

DEV CI TARGETS (owner 2026-08-26, "these should be params coming into the
lambda"): the {env: {account, region}} table is derived from route66's env
table (scripts/lib/r66) and passed as the DevCiTargetsJson stack parameter.
deploy_dev_ci_read_roles.py derives the same set from the same source.

Modes:  preflight | package | deploy
"""
import base64
import json
import os
import re
import subprocess
import sys

import boto3
import botocore
from cryptography.hazmat.primitives.asymmetric import ed25519
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, ".."))
ROUTE66_ROOT = os.environ.get("R66_ROUTE66_ROOT") or os.path.abspath(os.path.join(ROOT, "..", "route66"))
sys.path.insert(0, ROUTE66_ROOT)
from scripts.lib.r66 import account_id, all_env_names, is_prod, region as env_region  # noqa: E402

COMMAND_CENTER_ACCOUNT = "760773574016"
REGION = "us-east-1"
STACK_NAME = "CommandCenterSnapbot"
DEVOPS_STACK_NAME = "CommandCenterDevOpsBucket"
STORE_BUCKET = "r66-test-results-" + COMMAND_CENTER_ACCOUNT
DEVOPS_BUCKET = "core-devops-bucket-command-center-%s-%s" % (COMMAND_CENTER_ACCOUNT, REGION)
PROFILE = os.environ.get("AWS_PROFILE") or "command-center"
ORG_ID = "o-qpe4445sii"
SSM_PARAM = "/r66/evidence-attestor/private-key.v1"
ECR_REPOSITORY = "route66-snapbot"
CI_READ_ROLE_NAME = "route66-evidence-attestor-ci-read"

TEMPLATE = os.path.join(HERE, "evidence-attestor-template.yaml")
DEVOPS_TEMPLATE = os.path.join(HERE, "devops-bucket-template.yaml")
LAMBDA_SRC = os.path.join(HERE, "attestor", "index.js")
PUBLIC_KEY = os.path.join(HERE, "public-key.json")
BOOTSTRAP = os.path.join(ROUTE66_ROOT, "tmp", "command-center-evidence-attestor-bootstrap.json")


def session():
    return boto3.Session(profile_name=PROFILE, region_name=REGION)


def assert_command_center(sess):
    ident = sess.client("sts").get_caller_identity()
    if ident["Account"] != COMMAND_CENTER_ACCOUNT:
        sys.exit("REFUSING: profile %s resolves account %s, not command-center %s"
                 % (PROFILE, ident["Account"], COMMAND_CENTER_ACCOUNT))
    print("account OK: %s (%s)" % (ident["Account"], ident["Arn"]))


def read_text(path):
    with open(path, "r", encoding="utf-8") as fh:
        return fh.read()


def public_key_doc():
    return json.loads(read_text(PUBLIC_KEY))


def unwrap_key_b64():
    """The hard-coded unwrap key, read from the Lambda source so deploy and
    runtime can never disagree about it."""
    m = re.search(r'^const UNWRAP_KEY_B64 = "([A-Za-z0-9+/=]{44})";$', read_text(LAMBDA_SRC), re.M)
    if not m:
        sys.exit("REFUSING: attestor/index.js carries no 44-char UNWRAP_KEY_B64 constant")
    return m.group(1)


def dev_ci_targets():
    """{env: {account, region}} for every real dev env (<market>-dev; local-test is a
    workstation pseudo-env with account 000000000000), from the ONE env table."""
    return {
        env: {"account": account_id(env), "region": env_region(env)}
        for env in all_env_names() if env.endswith("-dev") and not is_prod(env)
    }


def ssm_string_value(sess):
    """The wrapped seed: existing SSM value, else the first-deploy bootstrap file."""
    ssm = sess.client("ssm")
    try:
        p = ssm.get_parameter(Name=SSM_PARAM, WithDecryption=False)["Parameter"]
        if p["Type"] != "String":
            sys.exit("REFUSING: %s exists as %s, expected plain String" % (SSM_PARAM, p["Type"]))
        print("private-key blob OK: %s exists (String)" % SSM_PARAM)
        return p["Value"], False
    except ssm.exceptions.ParameterNotFound:
        pass
    path = os.environ.get("R66_EVIDENCE_ATTESTOR_BOOTSTRAP") or BOOTSTRAP
    if not os.path.isfile(path):
        sys.exit("REFUSING: %s missing and no bootstrap file at %s" % (SSM_PARAM, path))
    doc = json.loads(read_text(path))
    if doc.get("ssm_parameter", SSM_PARAM) != SSM_PARAM:
        sys.exit("REFUSING: bootstrap parameter %s != expected %s" % (doc.get("ssm_parameter"), SSM_PARAM))
    print("bootstrap source OK: local tmp file (first deploy)")
    return doc["ssm_string_value"], True


def validate_seed_matches_public(wrapped_value):
    """Unwrap with the source constant and compare to repo public-key.json."""
    pub = public_key_doc()
    wrapped = json.loads(wrapped_value)
    aes = AESGCM(base64.b64decode(unwrap_key_b64()))
    pt = aes.decrypt(
        base64.b64decode(wrapped["nonce_b64"]),
        base64.b64decode(wrapped["ciphertext_b64"]),
        wrapped["aad"].encode("utf-8"),
    )
    key = json.loads(pt.decode("utf-8"))
    seed = base64.b64decode(key["private_seed_b64"])
    got_pub = ed25519.Ed25519PrivateKey.from_private_bytes(seed).public_key().public_bytes(
        encoding=serialization.Encoding.Raw, format=serialization.PublicFormat.Raw,
    )
    if key["key_id"] != pub["key_id"] or base64.b64encode(got_pub).decode("ascii") != pub["public_key_b64"]:
        sys.exit("REFUSING: wrapped private key does not match repo public-key.json")
    print("signing key OK: private seed derives to repo public key %s" % pub["key_id"])


def ensure_private_key_parameter(sess, value, create):
    if not create:
        return
    sess.client("ssm").put_parameter(
        Name=SSM_PARAM, Type="String", Value=value,
        Description="Wrapped Ed25519 private seed for command-center evidence attestor.",
    )
    print("private-key blob CREATED: %s (value not printed)" % SSM_PARAM)


def stack_status(cfn, name):
    try:
        return cfn.describe_stacks(StackName=name)["Stacks"][0]["StackStatus"]
    except botocore.exceptions.ClientError as exc:
        if "does not exist" in str(exc):
            return None
        raise


def apply(cfn, name, body, params, capabilities):
    kwargs = dict(StackName=name, TemplateBody=body, Parameters=params, Capabilities=capabilities)
    status = stack_status(cfn, name)
    if status is None:
        print("creating stack %s ..." % name)
        cfn.create_stack(OnFailure="DELETE", **kwargs)
        waiter = "stack_create_complete"
    else:
        print("stack %s exists (%s); updating ..." % (name, status))
        try:
            cfn.update_stack(**kwargs)
        except botocore.exceptions.ClientError as exc:
            if "No updates are to be performed" in str(exc):
                print("no changes to apply -- %s already matches the template" % name)
                return
            raise
        waiter = "stack_update_complete"
    cfn.get_waiter(waiter).wait(StackName=name, WaiterConfig={"Delay": 10, "MaxAttempts": 120})
    print("stack %s reached a terminal OK state" % name)


def ensure_devops_bucket(sess):
    apply(sess.client("cloudformation"), DEVOPS_STACK_NAME, read_text(DEVOPS_TEMPLATE), [], [])
    print("devops bucket OK: %s" % DEVOPS_BUCKET)


def package(sess=None):
    """Build and push the exact checkout as an immutable ECR image."""
    sess = sess or session()
    assert_command_center(sess)
    ensure_devops_bucket(sess)
    sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True, timeout=15).strip()
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        sys.exit("REFUSING: git rev-parse did not return a 40-hex SHA")
    registry = "%s.dkr.ecr.%s.amazonaws.com" % (COMMAND_CENTER_ACCOUNT, REGION)
    image = "%s/%s:%s" % (registry, ECR_REPOSITORY, sha)

    # Password travels on stdin and never appears in argv or logs. The build is
    # amd64 because the template declares x86_64 and Chromium pins that ABI.
    auth = sess.client("ecr").get_authorization_token()["authorizationData"][0]
    username, password = base64.b64decode(auth["authorizationToken"]).decode("utf-8").split(":", 1)
    subprocess.run(["docker", "login", "--username", username, "--password-stdin", registry],
                   input=password, text=True, check=True, timeout=60)
    subprocess.run(["docker", "build", "--platform", "linux/amd64", "-t", image, "."],
                   cwd=ROOT, check=True, timeout=3600)
    subprocess.run(["docker", "push", image], cwd=ROOT, check=True, timeout=1800)
    print("packaged snapbot image %s" % image)
    return image


def preflight():
    sess = session()
    assert_command_center(sess)
    cfn = sess.client("cloudformation")
    cfn.validate_template(TemplateBody=read_text(TEMPLATE))
    cfn.validate_template(TemplateBody=read_text(DEVOPS_TEMPLATE))
    print("templates OK")
    value, _ = ssm_string_value(sess)
    validate_seed_matches_public(value)
    print("dev CI targets: %s" % json.dumps(dev_ci_targets(), sort_keys=True))


def deploy():
    sess = session()
    assert_command_center(sess)
    value, create = ssm_string_value(sess)
    validate_seed_matches_public(value)
    ensure_private_key_parameter(sess, value, create)
    image_uri = package(sess)
    params = [
        {"ParameterKey": "ImageUri", "ParameterValue": image_uri},
        {"ParameterKey": "TestResultsBucketName", "ParameterValue": STORE_BUCKET},
        {"ParameterKey": "PrivateKeyParameterName", "ParameterValue": SSM_PARAM},
        {"ParameterKey": "DevCiTargetsJson", "ParameterValue": json.dumps(dev_ci_targets(), sort_keys=True)},
        {"ParameterKey": "CiReadRoleName", "ParameterValue": CI_READ_ROLE_NAME},
        {"ParameterKey": "OrganizationId", "ParameterValue": ORG_ID},
    ]
    apply(sess.client("cloudformation"), STACK_NAME, read_text(TEMPLATE), params, ["CAPABILITY_IAM"])


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    if mode == "preflight":
        preflight()
    elif mode == "package":
        package()
    elif mode == "deploy":
        deploy()
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
