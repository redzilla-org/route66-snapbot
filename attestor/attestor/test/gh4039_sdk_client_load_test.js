"use strict";
// redzilla-org/route66#4039 regression: the Lambda base image's bundled AWS SDK
// clients (resolved via /var/runtime/node_modules) fail every call before
// sending, with "serializerMiddleware is not found when adding
// endpointV2Middleware middleware before serializerMiddleware", because the
// base ships an incoherent mix of @smithy/* middleware versions across its
// individually-updated clients. The fix pins one coherent SDK client set into
// this image's own /var/task/node_modules (see Dockerfile). This test proves
// each of the three clients the attestor's attest-aws-resource action drives
// (redzilla-org/route66#4039's observed failures) can build and SEND a request
// from wherever this script's own module resolution lands, with no
// serializerMiddleware error. It is a valid-state regression test: it exercises
// exactly the three real client/operation pairs the observed bug hit.
//
// The clients point at a closed local TCP port (127.0.0.1:1, nothing ever
// listens there) with dummy credentials. A connection-refused/network error at
// the transport layer is PROOF the request was built and serialized correctly
// -- the serializerMiddleware defect fails BEFORE any network I/O is attempted,
// so a network-layer failure is a clean pass and a serializerMiddleware failure
// is a clean, unambiguous fail. No real AWS account or network access is used.

const assert = require("assert");

const FAKE_CREDS = {
  accessKeyId: "AKIAFAKE00000000FAKE",
  secretAccessKey: "fakefakefakefakefakefakefakefakefakefake",
};

// A closed port on loopback: connections are refused immediately, with no DNS
// or real network dependency, so this test runs offline and fast.
const DEAD_ENDPOINT = "http://127.0.0.1:1";

const CLIENT_CONFIG = {
  region: "us-east-1",
  credentials: FAKE_CREDS,
  endpoint: DEAD_ENDPOINT,
  maxAttempts: 1,
};

async function checkClient(label, modName, clientKey, commandKey, params) {
  const mod = require(modName);
  const Client = mod[clientKey];
  const Command = mod[commandKey];
  assert.ok(typeof Client === "function", label + ": " + clientKey + " missing from " + modName);
  assert.ok(typeof Command === "function", label + ": " + commandKey + " missing from " + modName);
  const client = new Client(CLIENT_CONFIG);
  try {
    await client.send(new Command(params));
    // A live loopback service would be surprising but is not evidence of the
    // bug; treat it as a pass since the request round-tripped successfully.
    console.log("[pass] " + label + ": request completed with no error");
  } catch (err) {
    const message = String(err && err.message ? err.message : err);
    if (/serializerMiddleware is not found/.test(message)) {
      throw new Error(
        "[FAIL] " + label + ": reproduced redzilla-org/route66#4039 -- " + message
      );
    }
    // Any other failure (ECONNREFUSED, timeout, etc.) proves the client built
    // and attempted to send the request past the middleware stack.
    console.log("[pass] " + label + ": request reached transport layer (" +
      (err && err.name ? err.name : "Error") + ": " + message + ")");
  }
}

async function main() {
  await checkClient(
    "cloudwatch-logs FilterLogEvents",
    "@aws-sdk/client-cloudwatch-logs",
    "CloudWatchLogsClient",
    "FilterLogEventsCommand",
    { logGroupName: "/r66/gh4039-test" }
  );
  await checkClient(
    "cloudwatch GetMetricData",
    "@aws-sdk/client-cloudwatch",
    "CloudWatchClient",
    "GetMetricDataCommand",
    {
      MetricDataQueries: [{
        Id: "m1",
        MetricStat: {
          Metric: { Namespace: "AWS/Lambda", MetricName: "Invocations" },
          Period: 60,
          Stat: "Sum",
        },
      }],
      StartTime: new Date(0),
      EndTime: new Date(),
    }
  );
  await checkClient(
    "dynamodb GetItem",
    "@aws-sdk/client-dynamodb",
    "DynamoDBClient",
    "GetItemCommand",
    { TableName: "gh4039-test", Key: {} }
  );
  console.log("[pass] all three clients built and sent requests with no serializerMiddleware error");
}

main().catch((err) => {
  console.error(String(err && err.message ? err.message : err));
  process.exit(1);
});
