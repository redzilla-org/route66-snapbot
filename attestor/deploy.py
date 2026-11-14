#!/usr/bin/env python3
"""Build and deploy the command-center route66-snapbot image.

boto3 only, fixed command-center account/region, immutable SHA image tag,
and live preflight checks before any stack mutation. The arm64 image is built
natively by the Graviton CodeBuild project route66-snapbot-arm64-image from a
`git archive` of the SHA (route66#4470); the laptop runs no docker for it.

KEY MATERIAL (owner 2026-08-26, "the AWS key should be hard-coded (not Lambda
env)"): the AES unwrap key is the UNWRAP_KEY_B64 constant in snapbot/src/attest.rs;
the wrapped Ed25519 seed is the plain SSM String /r66/evidence-attestor/
private-key.v1. This driver reads the constant out of attest.rs, unwraps the
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
import argparse
import base64
import json
import os
import re
import subprocess
import sys
import time

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
# Graviton CodeBuild project in devops-bucket-template.yaml (route66#4470).
BUILD_PROJECT = "route66-snapbot-arm64-image"
CI_READ_ROLE_NAME = "route66-evidence-attestor-ci-read"

TEMPLATE = os.path.join(HERE, "evidence-attestor-template.yaml")
DEVOPS_TEMPLATE = os.path.join(HERE, "devops-bucket-template.yaml")
# GH #4115: the handler is Rust; the unwrap key constant lives in attest.rs.
LAMBDA_SRC = os.path.join(ROOT, "snapbot", "src", "attest.rs")
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
    m = re.search(r'^const UNWRAP_KEY_B64: &str = "([A-Za-z0-9+/=]{44})";$', read_text(LAMBDA_SRC), re.M)
    if not m:
        sys.exit("REFUSING: snapbot/src/attest.rs carries no 44-char UNWRAP_KEY_B64 constant")
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
    # CAPABILITY_IAM: the stack owns the CodeBuild service role.
    apply(sess.client("cloudformation"), DEVOPS_STACK_NAME, read_text(DEVOPS_TEMPLATE), [], ["CAPABILITY_IAM"])
    print("devops bucket OK: %s" % DEVOPS_BUCKET)


def head_sha():
    sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True, timeout=15).strip()
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        sys.exit("REFUSING: git rev-parse did not return a 40-hex SHA")
    return sha


def image_arch(ecr, sha):
    """Architecture recorded in the pushed image's config blob; refuses an index
    (Lambda needs one plain manifest)."""
    img = ecr.batch_get_image(repositoryName=ECR_REPOSITORY, imageIds=[{"imageTag": sha}])["images"][0]
    manifest = json.loads(img["imageManifest"])
    if "config" not in manifest:
        sys.exit("REFUSING: %s is a manifest index, not a single image manifest" % sha)
    import urllib.request
    url = ecr.get_download_url_for_layer(repositoryName=ECR_REPOSITORY,
                                         layerDigest=manifest["config"]["digest"])["downloadUrl"]
    return json.load(urllib.request.urlopen(url, timeout=60))["architecture"], img["imageId"]["imageDigest"]


def package(sess=None):
    """Build the HEAD commit as an arm64 image NATIVELY on Graviton CodeBuild
    (route66#4470, owner: "Only CI does arm64", "no cross builds"). The laptop
    only stages `git archive` of the SHA; it never runs docker for the cloud image."""
    sess = sess or session()
    assert_command_center(sess)
    ensure_devops_bucket(sess)
    sha = head_sha()
    key = "snapbot-src/%s.zip" % sha
    archive = subprocess.check_output(["git", "archive", "--format=zip", sha], cwd=ROOT, timeout=300)
    sess.client("s3").put_object(Bucket=DEVOPS_BUCKET, Key=key, Body=archive)
    print("staged s3://%s/%s (%d bytes)" % (DEVOPS_BUCKET, key, len(archive)), flush=True)
    cb = sess.client("codebuild")
    build_id = cb.start_build(
        projectName=BUILD_PROJECT, sourceTypeOverride="S3", sourceLocationOverride="%s/%s" % (DEVOPS_BUCKET, key),
        environmentVariablesOverride=[{"name": "SNAPBOT_SHA", "value": sha, "type": "PLAINTEXT"}],
    )["build"]["id"]
    print("codebuild %s started" % build_id, flush=True)
    # Poll to a terminal status; the build is the whole arm64 compile.
    while True:
        build = cb.batch_get_builds(ids=[build_id])["builds"][0]
        if build["buildStatus"] != "IN_PROGRESS":
            break
        time.sleep(30)
    if build["buildStatus"] != "SUCCEEDED":
        sys.exit("REFUSING: codebuild %s ended %s; logs %s" % (build_id, build["buildStatus"],
                                                                build.get("logs", {}).get("deepLink")))
    arch, digest = image_arch(sess.client("ecr"), sha)
    if arch != "arm64":
        sys.exit("REFUSING: pushed image %s is %s, not arm64" % (digest, arch))
    print("packaged snapbot image %s:%s %s arch=%s" % (ECR_REPOSITORY, sha, digest, arch))
    return sha


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


def published_image(sess, sha):
    """Resolve an already-published build once; deployment never rebuilds its tag."""
    if sha is None:
        sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True, timeout=15).strip()
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        sys.exit("REFUSING: deployment requires a full 40-hex source SHA")
    ecr = sess.client("ecr")
    try:
        images = ecr.describe_images(repositoryName=ECR_REPOSITORY, imageIds=[{"imageTag": sha}])["imageDetails"]
    except ecr.exceptions.ImageNotFoundException:
        sys.exit("REFUSING: publish the tested snapbot image %s before deployment" % sha)
    if len(images) != 1 or not re.fullmatch(r"sha256:[0-9a-f]{64}", images[0].get("imageDigest", "")):
        sys.exit("REFUSING: ECR did not resolve one immutable snapbot image digest")
    image = "%s.dkr.ecr.%s.amazonaws.com/%s@%s" % (COMMAND_CENTER_ACCOUNT, REGION, ECR_REPOSITORY, images[0]["imageDigest"])
    print("deploying published snapbot source %s as %s" % (sha, image))
    return image


def deploy(sha=None):
    sess = session()
    assert_command_center(sess)
    value, create = ssm_string_value(sess)
    validate_seed_matches_public(value)
    # route66#4470: HEAD with no published image is built on Graviton CodeBuild
    # first; an explicit --sha must already be published.
    if sha is None:
        sha = head_sha()
        try:
            sess.client("ecr").describe_images(repositoryName=ECR_REPOSITORY, imageIds=[{"imageTag": sha}])
        except botocore.exceptions.ClientError as exc:
            if "ImageNotFound" not in str(exc):
                raise
            package(sess)
    # The stack declares arm64; refuse any image that is not.
    arch, _ = image_arch(sess.client("ecr"), sha)
    if arch != "arm64":
        sys.exit("REFUSING: image %s is %s, the stack runs arm64" % (sha, arch))
    image_uri = published_image(sess, sha)
    ensure_private_key_parameter(sess, value, create)
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
    # Tool-only commits can deploy an earlier tested build without rebuilding it under a new SHA.
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("preflight", "package", "deploy"))
    parser.add_argument("--sha", help="full source SHA of an already-published image (deploy only; default HEAD)")
    args = parser.parse_args()
    if args.sha is not None and args.mode != "deploy":
        parser.error("--sha is supported only by deploy")
    mode = args.mode
    if mode == "preflight":
        preflight()
    elif mode == "package":
        package()
    elif mode == "deploy":
        deploy(args.sha)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
