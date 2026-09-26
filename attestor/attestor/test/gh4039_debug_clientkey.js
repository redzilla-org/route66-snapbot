"use strict";
// redzilla-org/route66#4039: loadServiceClient's clientKey heuristic
// (index.js `Object.keys(mod).find((k) => k !== "Client" && /Client$/.test(k)
// && typeof mod[k] === "function")`) picks the FIRST export ending in
// "Client" that is not literally named "Client". Every @aws-sdk/client-*
// package also re-exports its internal smithy base class under a mangled
// name ("__Client") that also matches that filter and sorts before the
// concrete named client class in Object.keys() enumeration order. The
// heuristic therefore always resolves to the abstract base client, never the
// concrete one (DynamoDBClient, CloudWatchClient, ...), which lacks the
// concrete client's endpoint/serde middleware wiring and throws exactly the
// reported "serializerMiddleware is not found when adding endpointV2Middleware
// middleware before serializerMiddleware" error on every call -- independent
// of which SDK version set is installed. This is a valid-state regression
// test for that heuristic, not a test of invalid input.
const assert = require("assert");

function pickClientKeyLikeLoadServiceClient(mod) {
  return Object.keys(mod).find((k) => k !== "Client" && /Client$/.test(k) && typeof mod[k] === "function");
}

for (const [pkg, expectedClassName] of [
  ["client-dynamodb", "DynamoDBClient"],
  ["client-cloudwatch", "CloudWatchClient"],
  ["client-cloudwatch-logs", "CloudWatchLogsClient"],
]) {
  const mod = require(require.resolve("@aws-sdk/" + pkg));
  const clientKey = pickClientKeyLikeLoadServiceClient(mod);
  assert.strictEqual(
    clientKey,
    expectedClassName,
    "loadServiceClient's clientKey heuristic must resolve @aws-sdk/" + pkg +
      " to " + expectedClassName + ", got " + clientKey +
      " (all Client-ending exports: " + Object.keys(mod).filter((k) => /Client$/.test(k)).join(",") + ")"
  );
}
console.log("[pass] loadServiceClient's clientKey heuristic resolves the concrete client class for all three services");
