"use strict";
// redzilla-org/route66#4039: loadServiceClient's class-selection must resolve
// the CONCRETE named client (DynamoDBClient, CloudWatchClient,
// CloudWatchLogsClient), never the smithy base class every @aws-sdk/client-*
// package also re-exports as "__Client" under the same "/Client$/" suffix.
// The base class lacks the concrete client's endpoint/serde middleware wiring
// and throws exactly the reported "serializerMiddleware is not found when
// adding endpointV2Middleware middleware before serializerMiddleware" error on
// every call, independent of which SDK version set is installed.
//
// This drives the REAL entry point, exports.handler(action:
// "attest-aws-resource"), for all three services the ticket names, faking
// only the external AWS boundary: STS GetCallerIdentity is stubbed to succeed
// (the real code's caller-identity step always runs first), and the target
// service client is stubbed to reject the way a real AWS error response
// would. A wrong class selection (the pre-fix "__Client") never reaches that
// stub at all -- its own unpatched send() throws the serializerMiddleware
// error instead -- so this test fails loudly on the old heuristic and passes
// only once loadServiceClient truly resolves the concrete class.
//
// RED-BEFORE CHECK (kept as its own assertion, not just narrative): the old
// heuristic is reproduced inline and asserted to pick the wrong export, so a
// future regression that reintroduces a "/Client$/ suffix scan" is caught
// even if it does not literally restore this file's old code.
const assert = require("assert");

process.env.EVIDENCE_ATTESTOR_REGION = "us-east-1";
process.env.EVIDENCE_ATTESTOR_BUCKET = "gh4039-test-bucket";
process.env.EVIDENCE_ATTESTOR_SSM_PARAM = "/r66/gh4039-test/signing-key";
process.env.EVIDENCE_ATTESTOR_CI_READ_ROLE_NAME = "gh4039-test-ci-read-role";
process.env.EVIDENCE_ATTESTOR_DEV_CI_JSON = "{}";

// --- Red-before check: the OLD "/Client$/ suffix scan" heuristic must still
// pick the wrong ("__Client") export for every service this ticket covers.
// This is evidence the bug this test guards against is real and would
// reproduce if the old selection method ever came back.
function oldSuffixScanHeuristic(mod) {
  return Object.keys(mod).find((k) => k !== "Client" && /Client$/.test(k) && typeof mod[k] === "function");
}
for (const [pkg, concreteClassName] of [
  ["client-dynamodb", "DynamoDBClient"],
  ["client-cloudwatch", "CloudWatchClient"],
  ["client-cloudwatch-logs", "CloudWatchLogsClient"],
]) {
  const mod = require(require.resolve("@aws-sdk/" + pkg));
  const oldPick = oldSuffixScanHeuristic(mod);
  assert.strictEqual(
    oldPick,
    "__Client",
    "red-before check: expected the old suffix-scan heuristic to still mispick " +
      "'__Client' for @aws-sdk/" + pkg + " (got " + oldPick + "); if this no longer " +
      "reproduces, the base @aws-sdk/client-* export shape changed and this test's " +
      "premise needs re-verifying"
  );
  assert.notStrictEqual(
    oldPick,
    concreteClassName,
    "red-before check: the old heuristic must NOT accidentally match the concrete class"
  );
}

// --- Green-after check: drive the REAL loadServiceClient/attestAwsResource
// path (via exports.handler) for all three services and assert each reaches
// the concrete client's stubbed send(), never the abstract base class's real
// (and broken) one.
const { DynamoDBClient, GetItemCommand } = require(require.resolve("@aws-sdk/client-dynamodb"));
const { CloudWatchClient, GetMetricDataCommand } = require(require.resolve("@aws-sdk/client-cloudwatch"));
const { CloudWatchLogsClient, FilterLogEventsCommand } = require(require.resolve("@aws-sdk/client-cloudwatch-logs"));
const { S3Client, PutObjectCommand } = require(require.resolve("@aws-sdk/client-s3"));
const { SSMClient } = require(require.resolve("@aws-sdk/client-ssm"));
const { STSClient, GetCallerIdentityCommand } = require(require.resolve("@aws-sdk/client-sts"));

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

// These must NEVER be called: no evidence object, no signing, no GitHub post
// on the (deliberately induced) failure path.
S3Client.prototype.send = async function (command) {
  throw new Error("gh4039 test stub: S3 must not be called on this failure path");
};
SSMClient.prototype.send = async function () {
  throw new Error("gh4039 test stub: SSM (signing key) must not be fetched on this failure path");
};
const originalFetch = global.fetch;
global.fetch = async function () {
  throw new Error("gh4039 test stub: GitHub must not be contacted on this failure path");
};

const { handler } = require("../index.js");

const CASES = [
  {
    label: "dynamodb GetItem",
    service: "dynamodb",
    operation: "GetItem",
    params: { TableName: "gh4039-test", Key: {} },
    Client: DynamoDBClient,
    Command: GetItemCommand,
    concreteClassName: "DynamoDBClient",
  },
  {
    label: "cloudwatch GetMetricData",
    service: "cloudwatch",
    operation: "GetMetricData",
    params: {
      MetricDataQueries: [{
        Id: "m1",
        MetricStat: { Metric: { Namespace: "AWS/Lambda", MetricName: "Invocations" }, Period: 60, Stat: "Sum" },
      }],
      StartTime: new Date(0),
      EndTime: new Date(),
    },
    Client: CloudWatchClient,
    Command: GetMetricDataCommand,
    concreteClassName: "CloudWatchClient",
  },
  {
    label: "cloudwatch-logs FilterLogEvents",
    service: "cloudwatch-logs",
    operation: "FilterLogEvents",
    params: { logGroupName: "/r66/gh4039-test" },
    Client: CloudWatchLogsClient,
    Command: FilterLogEventsCommand,
    concreteClassName: "CloudWatchLogsClient",
  },
];

async function runCase(testCase) {
  let calls = 0;
  testCase.Client.prototype.send = async function (command) {
    calls++;
    if (command instanceof testCase.Command) {
      const err = new Error("Requested resource not found");
      err.name = "ResourceNotFoundException";
      err.$metadata = { httpStatusCode: 400 };
      throw err;
    }
    throw new Error("unexpected " + testCase.concreteClassName + " command in gh4039 test stub");
  };

  const event = {
    action: "attest-aws-resource",
    env: "chicago-dev",
    region: "us-east-1",
    service: testCase.service,
    operation: testCase.operation,
    params: testCase.params,
    credentials: {
      accessKeyId: "AKIAFAKE00000000FAKE",
      secretAccessKey: "fakefakefakefakefakefakefakefakefakefake",
      sessionToken: "faketoken",
    },
    intent: "GH #4039 regression test: attestAwsResource resolves the concrete client",
    category: "BEFORE",
    github: { issue: 4039, token: "fake-invocation-only-token" },
  };

  let thrown = null;
  try {
    await handler(event);
  } catch (err) {
    thrown = err;
  }

  assert.ok(thrown, testCase.label + ": handler must throw when the underlying AWS call errors");
  const message = String(thrown && thrown.message ? thrown.message : thrown);
  assert.ok(
    !/serializerMiddleware is not found/.test(message),
    testCase.label + ": reproduced redzilla-org/route66#4039 (wrong class selected) -- " + message
  );
  assert.ok(
    message.startsWith("FAILURE TO ATTEST"),
    testCase.label + ": expected FAILURE TO ATTEST, got: " + message
  );
  assert.strictEqual(
    calls, 1,
    testCase.label + ": the concrete " + testCase.concreteClassName +
      " client's send() must have been invoked exactly once (proves loadServiceClient " +
      "resolved the concrete class, not the abstract base)"
  );
  console.log("[pass] " + testCase.label + ": loadServiceClient resolved " + testCase.concreteClassName);
}

async function main() {
  for (const testCase of CASES) {
    await runCase(testCase);
  }
  global.fetch = originalFetch;
  console.log("[pass] loadServiceClient resolves the concrete client class for all three services");
}

main().catch((err) => {
  console.error(String(err && err.message ? err.message : err));
  process.exit(1);
});
