"use strict";
// redzilla-org/route66#4039 (behavior b): attestAwsResource must throw
// "FAILURE TO ATTEST" when the AWS SDK call it makes on the caller's behalf
// returns an error, and must never S3-put, sign, or publish to GitHub in that
// case (index.js ~L1889-1908). This drives the REAL entry point,
// exports.handler with action "attest-aws-resource", exactly as
// attest-aws-resource callers invoke it. Only the external AWS/network
// boundary is faked: STS GetCallerIdentity is stubbed to succeed (so the
// caller-identity step the real code always runs first does not become the
// thing that fails), the target DynamoDB client is stubbed to reject the way
// a real AWS error response would, and S3/SSM/fetch are spied on to prove no
// evidence object, signature, or GitHub comment is ever produced.

const assert = require("assert");

// Required config the module reads at load time (index.js `CFG`). Values are
// arbitrary placeholders; no real AWS resource is touched by this test.
process.env.EVIDENCE_ATTESTOR_REGION = "us-east-1";
process.env.EVIDENCE_ATTESTOR_BUCKET = "gh4039-test-bucket";
process.env.EVIDENCE_ATTESTOR_SSM_PARAM = "/r66/gh4039-test/signing-key";
process.env.EVIDENCE_ATTESTOR_CI_READ_ROLE_NAME = "gh4039-test-ci-read-role";
process.env.EVIDENCE_ATTESTOR_DEV_CI_JSON = "{}";

// Resolve the exact same way index.js's loadServiceClient does (require.resolve
// then require(resolved)), so the class we patch is guaranteed to be the exact
// class attestAwsResource constructs -- not merely a same-named class loaded
// from a second module-cache entry.
const dynamodbMod = require(require.resolve("@aws-sdk/client-dynamodb"));
const { DynamoDBClient, GetItemCommand } = dynamodbMod;
const { S3Client, PutObjectCommand } = require(require.resolve("@aws-sdk/client-s3"));
const { SSMClient } = require(require.resolve("@aws-sdk/client-ssm"));
const { STSClient, GetCallerIdentityCommand } = require(require.resolve("@aws-sdk/client-sts"));

let s3PutCalls = 0;
let ssmCalls = 0;
let dynamoCalls = 0;
let fetchCalls = 0;

// Fake the external AWS boundary only: STS succeeds (the real code's
// caller-identity check always runs first and must not be what fails here),
// DynamoDB rejects the way a real service error would.
STSClient.prototype.send = async function (command) {
  if (command instanceof GetCallerIdentityCommand) {
    return {
      Arn: "arn:aws:sts::123456789012:assumed-role/gh4039-test/session",
      Account: "123456789012",
      UserId: "AROAFAKE:session",
      $metadata: { httpStatusCode: 200 },
    };
  }
  throw new Error("unexpected STS command in gh4039 test stub");
};

DynamoDBClient.prototype.send = async function (command) {
  dynamoCalls++;
  if (command instanceof GetItemCommand) {
    const err = new Error("Requested resource not found: Table: gh4039-test not found");
    err.name = "ResourceNotFoundException";
    err.$metadata = { httpStatusCode: 400 };
    throw err;
  }
  throw new Error("unexpected DynamoDB command in gh4039 test stub");
};

// These three must NEVER be called on the failure path: no evidence object is
// written (S3 put), no signing key is fetched (SSM get), no GitHub comment is
// posted (fetch).
S3Client.prototype.send = async function (command) {
  if (command instanceof PutObjectCommand) s3PutCalls++;
  throw new Error("gh4039 test stub: S3 must not be called on the attestAwsResource failure path");
};
SSMClient.prototype.send = async function () {
  ssmCalls++;
  throw new Error("gh4039 test stub: SSM (signing key) must not be fetched on the failure path");
};
const originalFetch = global.fetch;
global.fetch = async function (...args) {
  fetchCalls++;
  throw new Error("gh4039 test stub: GitHub must not be contacted on the failure path");
};

const { handler } = require("../index.js");

async function main() {
  const event = {
    action: "attest-aws-resource",
    env: "chicago-dev",
    region: "us-east-1",
    service: "dynamodb",
    operation: "GetItem",
    params: { TableName: "gh4039-test", Key: {} },
    credentials: {
      accessKeyId: "AKIAFAKE00000000FAKE",
      secretAccessKey: "fakefakefakefakefakefakefakefakefakefake",
      sessionToken: "faketoken",
    },
    intent: "GH #4039 regression test: attest-aws-resource fails closed",
    category: "BEFORE",
    github: { issue: 4039, token: "fake-invocation-only-token" },
  };

  let thrown = null;
  try {
    await handler(event);
  } catch (err) {
    thrown = err;
  }

  assert.ok(thrown, "handler must throw when the underlying AWS call errors");
  const message = String(thrown && thrown.message ? thrown.message : thrown);
  console.log("[debug] thrown message: " + message);
  console.log("[debug] dynamoCalls=" + dynamoCalls + " s3PutCalls=" + s3PutCalls +
    " ssmCalls=" + ssmCalls + " fetchCalls=" + fetchCalls);
  assert.ok(
    message.startsWith("FAILURE TO ATTEST"),
    "expected FAILURE TO ATTEST, got: " + message
  );
  assert.strictEqual(dynamoCalls, 1, "the underlying DynamoDB call should have been attempted exactly once");
  assert.strictEqual(s3PutCalls, 0, "no S3 put must occur when the AWS call errors");
  assert.strictEqual(ssmCalls, 0, "no signing (SSM key fetch) must occur when the AWS call errors");
  assert.strictEqual(fetchCalls, 0, "no GitHub post must occur when the AWS call errors");

  console.log("[pass] attestAwsResource threw FAILURE TO ATTEST with no S3 put, no signing, no GitHub post");
  global.fetch = originalFetch;
}

main().catch((err) => {
  console.error(String(err && err.message ? err.message : err));
  process.exit(1);
});
